use circuit::{Circuit, Gate};
use rand::Rng;
use wire::Wire;

use itertools::Itertools;

use std::collections::HashMap;

type GarbledGate = Vec<u128>;

pub struct Garbler {
    deltas     : HashMap<u16, Wire>,
    inputs     : Vec<Wire>,
    consts     : Vec<Wire>,
    outputs    : Vec<Vec<u128>>,
    rng        : Rng,
}

pub struct Evaluator {
    gates  : Vec<GarbledGate>,
    consts : Vec<Wire>,
}

pub fn garble(c: &Circuit) -> (Garbler, Evaluator) {
    let mut gb = Garbler::new();

    for &m in c.gate_moduli.iter().unique() {
        gb.create_delta(m);
    }

    let mut wires: Vec<Wire> = Vec::with_capacity(c.gates.len());
    let mut gates: Vec<GarbledGate> = Vec::with_capacity(c.num_nonfree_gates);

    for i in 0..c.gates.len() {
        let q = c.modulus(i);
        let w = match c.gates[i] {
            Gate::Input { .. } => gb.input(q),
            Gate::Const { .. } => gb.constant(q),

            Gate::Add { xref, yref } => wires[xref].plus(&wires[yref]),
            Gate::Sub { xref, yref } => wires[xref].minus(&wires[yref]),
            Gate::Cmul { xref, c }   => wires[xref].cmul(c),

            Gate::Proj { xref, ref tt, .. }  => {
                let X = &wires[xref];
                let (w,g) = gb.proj(X, q, tt, i);
                gates.push(g);
                w
            }

            Gate::Yao { xref, yref, ref tt, .. } => {
                let X = &wires[xref];
                let Y = &wires[yref];
                let (w,g) = gb.yao(X, Y, q, tt, i);
                gates.push(g);
                w
            }

            Gate::HalfGate { xref, yref, .. }  => {
                let X = &wires[xref];
                let Y = &wires[yref];
                let (w,g) = gb.half_gate(X, Y, i);
                gates.push(g);
                w
            }
        };
        wires.push(w); // add the new zero-wire
    }
    for (i, &r) in c.output_refs.iter().enumerate() {
        let X = wires[r].clone();
        gb.output(&X, i);
    }

    let cs = c.const_vals.as_ref().expect("constants needed!");
    let ev = Evaluator::new(gates, gb.encode_consts(cs));
    (gb, ev)
}

#[allow(non_snake_case)]
impl Garbler {
    pub fn new() -> Self {
        Garbler {
            deltas: HashMap::new(),
            inputs: Vec::new(),
            consts: Vec::new(),
            outputs: Vec::new(),
            rng: Rng::new(),
        }
    }

    fn create_delta(&mut self, q: u16) -> Wire {
        if !self.deltas.contains_key(&q) {
            let w = Wire::rand_delta(&mut self.rng, q);
            self.deltas.insert(q, w.clone());
            w
        } else {
            self.deltas[&q].clone()
        }
    }

    fn delta(&self, q:u16) -> &Wire {
        &self.deltas.get(&q).expect("garbler has not generated delta!")
    }

    pub fn input(&mut self, q: u16) -> Wire {
        let w = Wire::rand(&mut self.rng, q);
        self.inputs.push(w.clone());
        w
    }

    pub fn constant(&mut self, q: u16) -> Wire {
        let w = Wire::rand(&mut self.rng, q);
        self.consts.push(w.clone());
        w
    }

    pub fn output(&mut self, X: &Wire, output_num: usize) {
        let mut cts = Vec::new();
        {
            let q = X.modulus();
            let D = self.delta(q);
            for k in 0..q {
                let t = output_tweak(output_num, k);
                cts.push(X.plus(&D.cmul(k)).hash(t));
            }
        }
        self.outputs.push(cts);
    }

    pub fn proj(&self, A: &Wire, q_out: u16, tt: &[u16], gate_num: usize)
        -> (Wire, GarbledGate)
    {
        let q_in = A.modulus();
        // we have to fill in the vector in an unkonwn order because of the
        // color bits. Since some of the values in gate will be void
        // temporarily, we use Vec<Option<..>>
        let mut gate = vec![None; q_in as usize - 1];

        let tao = A.color();        // input zero-wire
        let g = tweak(gate_num);    // gate tweak

        // output zero-wire
        // W_g^0 <- -H(g, W_{a_1}^0 - \tao\Delta_m) - \phi(-\tao)\Delta_n
        let C = A.minus(&self.delta(q_in).cmul(tao))
                 .hashback(g, q_out)
                 .minus(&self.delta(q_out).cmul(tt[((q_in - tao) % q_in) as usize]));

        for x in 0..q_in {
            let ix = (tao as usize + x as usize) % q_in as usize;
            if ix == 0 { continue }
            let A_ = A.plus(&self.delta(q_in).cmul(x));
            let C_ = C.plus(&self.delta(q_out).cmul(tt[x as usize]));
            let ct = A_.hash(g) ^ C_.as_u128();
            gate[ix - 1] = Some(ct);
        }

        // unwrap the Option elems inside the Vec
        let gate = gate.into_iter().map(Option::unwrap).collect();
        (C, gate)
    }

    fn yao(&self, A: &Wire, B: &Wire, q: u16, tt: &[Vec<u16>], gate_num: usize)
        -> (Wire, GarbledGate)
    {
        let xmod = A.modulus() as usize;
        let ymod = B.modulus() as usize;
        let mut gate = vec![None; xmod * ymod - 1];

        // gate tweak
        let g = tweak(gate_num);

        // sigma is the output truth value of the 0,0-colored wirelabels
        let sigma = tt[((xmod - A.color() as usize) % xmod) as usize]
                      [((ymod - B.color() as usize) % ymod) as usize];

        // we use the row reduction trick here
        let B_delta = self.delta(ymod as u16);
        let C = A.minus(&self.delta(xmod as u16).cmul(A.color()))
                 .hashback2(&B.minus(&B_delta.cmul(B.color())), g, q)
                 .minus(&self.delta(q).cmul(sigma));

        for x in 0..xmod {
            let A_ = A.plus(&self.delta(xmod as u16).cmul(x as u16));
            for y in 0..ymod {
                let ix = ((A.color() as usize + x) % xmod) * ymod +
                         ((B.color() as usize + y) % ymod);
                if ix == 0 { continue }
                debug_assert_eq!(gate[ix-1], None);
                let B_ = B.plus(&self.delta(ymod as u16).cmul(y as u16));
                let C_ = C.plus(&self.delta(q).cmul(tt[x][y]));
                let ct = A_.hash2(&B_,g) ^ C_.as_u128();
                gate[ix-1] = Some(ct);
            }
        }
        let gate = gate.into_iter().map(Option::unwrap).collect();
        (C, gate)
    }

    pub fn half_gate(&self, A: &Wire, B: &Wire, gate_num: usize)
        -> (Wire, GarbledGate)
    {
        let q = A.modulus();
        let qb = B.modulus();

        debug_assert!(q >= qb);

        let mut gate = vec![None; q as usize + qb as usize - 2];
        let g = tweak(gate_num);

        let r = B.color(); // secret value known only to the garbler (ev knows r+b)

        let D = self.delta(q);
        let Db = self.delta(qb);

        // X = H(A+aD) + arD such that a + A.color == 0
        let alpha = (q - A.color()) % q; // alpha = -A.color
        let X = A.plus(&D.cmul(alpha)).hashback(g,q)
                 .plus(&D.cmul((alpha * r) % q));

        // Y = H(B + bD)
        let beta = (qb - B.color()) % qb;
        let Y = B.plus(&Db.cmul(beta)).hashback(g,q);

        for a in 0..q {
            // garbler's half-gate: outputs X-arD
            // G = H(A+aD) + X+a(-r)D = H(A+aD) + X-arD
            let A_ = A.plus(&D.cmul(a));
            if A_.color() != 0 {
                let tao = a * (q - r) % q;
                let G = A_.hash(g) ^ X.plus(&D.cmul(tao)).as_u128();
                gate[A_.color() as usize - 1] = Some(G);
            }
        }

        for b in 0..qb {
            // evaluator's half-gate: outputs Y+a(r+b)D
            // G = H(B+bD) + Y-(b+r)A
            let B_ = B.plus(&Db.cmul(b));
            if B_.color() != 0 {
                let G = B_.hash(g) ^ Y.minus(&A.cmul((b+r)%qb)).as_u128();
                gate[q as usize - 1 + B_.color() as usize - 1] = Some(G);
            }
        }

        let gate = gate.into_iter().map(Option::unwrap).collect();
        (X.plus(&Y), gate) // output zero wire
    }

    pub fn encode_consts(&self, consts: &[u16]) -> Vec<Wire> {
        debug_assert_eq!(consts.len(), self.consts.len(), "[encode_consts] not enough consts!");
        let mut xs = Vec::new();
        for i in 0..consts.len() {
            let x = consts[i];
            let X = self.consts[i].clone();
            let D = self.deltas[&X.modulus()].clone();
            xs.push(X.plus(&D.cmul(x)));
        }
        xs
    }

    pub fn encode(&self, inputs: &[u16]) -> Vec<Wire> {
        debug_assert_eq!(inputs.len(), self.inputs.len());
        let mut xs = Vec::new();
        for i in 0..inputs.len() {
            let x = inputs[i];
            let X = self.inputs[i].clone();
            let D = self.deltas[&X.modulus()].clone();
            xs.push(X.plus(&D.cmul(x)));
        }
        xs
    }

    pub fn decode(&self, ws: &[Wire]) -> Vec<u16> {
        debug_assert_eq!(ws.len(), self.outputs.len());
        let mut outs = Vec::new();
        for i in 0..ws.len() {
            let q = ws[i].modulus();
            for k in 0..q {
                let h = ws[i].hash(output_tweak(i,k));
                if h == self.outputs[i][k as usize] {
                    outs.push(k);
                    break;
                }
            }
        }
        debug_assert_eq!(ws.len(), outs.len(), "decoding failed");
        outs
    }
}

#[allow(non_snake_case)]
impl Evaluator {
    pub fn new(gates: Vec<GarbledGate>, consts: Vec<Wire>) -> Self {
        Evaluator { gates, consts }
    }

    pub fn size(&self) -> usize {
        let mut c = self.consts.len();
        for g in self.gates.iter() {
            c += g.len();
        }
        c
    }

    pub fn eval(&self, c: &Circuit, inputs: &[Wire]) -> Vec<Wire> {
        let mut wires: Vec<Wire> = Vec::new();
        for i in 0..c.gates.len() {
            let q = c.modulus(i);
            let w = match c.gates[i] {

                Gate::Input { id }       => inputs[id].clone(),
                Gate::Const { id, .. }   => self.consts[id].clone(),
                Gate::Add { xref, yref } => wires[xref].plus(&wires[yref]),
                Gate::Sub { xref, yref } => wires[xref].minus(&wires[yref]),
                Gate::Cmul { xref, c }   => wires[xref].cmul(c),

                Gate::Proj { xref, id, .. } => {
                    let x = &wires[xref];
                    if x.color() == 0 {
                        x.hashback(i as u128, q)
                    } else {
                        let ct = self.gates[id][x.color() as usize - 1];
                        Wire::from_u128(ct ^ x.hash(i as u128), q)
                    }
                }

                Gate::Yao { xref, yref, id, .. } => {
                    let a = &wires[xref];
                    let b = &wires[yref];
                    if a.color() == 0 && b.color() == 0 {
                        a.hashback2(&b, tweak(i), q)
                    } else {
                        let ix = a.color() as usize * c.modulus(yref) as usize + b.color() as usize;
                        let ct = self.gates[id][ix - 1];
                        Wire::from_u128(ct ^ a.hash2(&b, tweak(i)), q)
                    }
                }

                Gate::HalfGate { xref, yref, id } => {
                    let g = tweak(i);

                    // garbler's half gate
                    let A = &wires[xref];
                    let L = if A.color() == 0 {
                        A.hashback(g,q)
                    } else {
                        let ct_left = self.gates[id][A.color() as usize - 1];
                        Wire::from_u128(ct_left ^ A.hash(g), q)
                    };

                    // evaluator's half gate
                    let B = &wires[yref];
                    let R = if B.color() == 0 {
                        B.hashback(g,q)
                    } else {
                        let ct_right = self.gates[id][(q + B.color()) as usize - 2];
                        Wire::from_u128(ct_right ^ B.hash(g), q)
                    };
                    L.plus(&R.plus(&A.cmul(B.color())))
                }
            };
            wires.push(w);
        }

        c.output_refs.iter().map(|&r| {
            wires[r].clone()
        }).collect()
    }
}

fn tweak(i: usize) -> u128 {
    i as u128
}

fn output_tweak(i: usize, k: u16) -> u128 {
    let (left, _) = (i as u128).overflowing_shl(64);
    left + k as u128
}


#[cfg(test)]
mod tests {
    use super::*;
    use circuit::{Circuit, Builder};
    use rand::Rng;
    use numbers;
    use util::IterToVec;

    // helper {{{
    fn garble_test_helper<F>(f: F)
        where F: Fn(u16) -> Circuit
    {
        let mut rng = Rng::new();
        for _ in 0..16 {
            let q = rng.gen_prime();
            let c = &f(q);
            let (gb, ev) = garble(&c);
            println!("number of ciphertexts for mod {}: {}", q, ev.size());
            for _ in 0..64 {
                let inps = (0..c.ninputs()).map(|i| { rng.gen_u16() % c.input_mod(i) }).to_vec();
                let xs = &gb.encode(&inps);
                let ys = &ev.eval(c, xs);
                let decoded = gb.decode(ys)[0];
                let should_be = c.eval(&inps)[0];
                if decoded != should_be {
                    println!("inp={:?} q={} got={} should_be={}", inps, q, decoded, should_be);
                    panic!("failed test!");
                }
            }
        }
    }
//}}}
    #[test] // add {{{
    fn add() {
        garble_test_helper(|q| {
            let mut b = Builder::new();
            let x = b.input(q);
            let y = b.input(q);
            let z = b.add(x,y);
            b.output(z);
            b.finish()
        });
    }
//}}}
    #[test] // add_many {{{
    fn add_many() {
        garble_test_helper(|q| {
            let mut b = Builder::new();
            let xs = b.inputs(16, q);
            let z = b.add_many(&xs);
            b.output(z);
            b.finish()
        });
    }
//}}}
    #[test] // or_many {{{
    fn or_many() {
        garble_test_helper(|_| {
            let mut b = Builder::new();
            let xs = b.inputs(16, 2);
            let z = b.or_many(&xs);
            b.output(z);
            b.finish()
        });
    }
//}}}
    #[test] // and_many {{{
    fn and_many() {
        garble_test_helper(|_| {
            let mut b = Builder::new();
            let xs = b.inputs(16, 2);
            let z = b.and_many(&xs);
            b.output(z);
            b.finish()
        });
    }
//}}}
    #[test] // sub {{{
    fn sub() {
        garble_test_helper(|q| {
            let mut b = Builder::new();
            let x = b.input(q);
            let y = b.input(q);
            let z = b.sub(x,y);
            b.output(z);
            b.finish()
        });
    }
//}}}
    #[test] // cmul {{{
    fn cmul() {
        garble_test_helper(|q| {
            let mut b = Builder::new();
            let x = b.input(q);
            let _ = b.input(q);
            let z;
            if q > 2 {
                z = b.cmul(x, 2);
            } else {
                z = b.cmul(x, 1);
            }
            b.output(z);
            b.finish()
        });
    }
//}}}
    #[test] // proj_cycle {{{
    fn proj_cycle() {
        garble_test_helper(|q| {
            let mut tab = Vec::new();
            for i in 0..q {
                tab.push((i + 1) % q);
            }
            let mut b = Builder::new();
            let x = b.input(q);
            let _ = b.input(q);
            let z = b.proj(x, q, tab);
            b.output(z);
            b.finish()
        });
    }
//}}}
    #[test] // proj_rand {{{
    fn proj_rand() {
        garble_test_helper(|q| {
            let mut rng = Rng::new();
            let mut tab = Vec::new();
            for _ in 0..q {
                tab.push(rng.gen_u16() % q);
            }
            let mut b = Builder::new();
            let x = b.input(q);
            let _ = b.input(q);
            let z = b.proj(x, q, tab);
            b.output(z);
            b.finish()
        });
    }
//}}}
    #[test] // mod_change {{{
    fn mod_change() {
        garble_test_helper(|q| {
            let mut b = Builder::new();
            let x = b.input(q);
            let z = b.mod_change(x,q*2);
            b.output(z);
            b.finish()
        });
    }
//}}}
    #[test] // yao {{{
    fn yao() {
        garble_test_helper(|q| {
            let mut b = Builder::new();
            let x = b.input(q);
            let y = b.input(q);
            let mut tt = Vec::new();
            for a in 0..q {
                let mut tt_ = Vec::new();
                for b in 0..q {
                    tt_.push(a * b % q);
                }
                tt.push(tt_);
            }
            let z = b.yao(x, y, q, tt);
            b.output(z);
            b.finish()
        });
    }
//}}}
    #[test] // half_gate {{{
    fn half_gate() {
        garble_test_helper(|q| {
            let mut b = Builder::new();
            let x = b.input(q);
            let y = b.input(q);
            let z = b.half_gate(x,y);
            b.output(z);
            b.finish()
        });
    }
//}}}
    #[test] // half_gate_unequal_mods {{{
    fn half_gate_unequal_mods() {
        garble_test_helper(|q| {
            let mut b = Builder::new();
            let x = b.input(q);
            let y = b.input(2);
            let z = b.half_gate(x,y);
            b.output(z);
            b.finish()
        });
    }
//}}}
    #[test] // base_q_addition_no_carry {{{
    fn base_q_addition_no_carry() {
        garble_test_helper(|q| {
            let mut b = Builder::new();
            let n = 16;
            let xs = b.inputs(n,q);
            let ys = b.inputs(n,q);
            let zs = b.addition_no_carry(&xs, &ys);
            b.outputs(&zs);
            b.finish()
        });
    }
//}}}
    #[test] // fancy_addition {{{
    fn fancy_addition() {
        let mut rng = Rng::new();

        let nargs = 2 + rng.gen_usize() % 100;
        let mods = (0..7).map(|_| rng.gen_modulus()).to_vec();
        // let nargs = 97;
        // let mods = [37,10,10,54,100,51,17];

        let mut b = Builder::new();
        let xs = (0..nargs).map(|_| {
            mods.iter().map(|&q| b.input(q)).to_vec()
        }).to_vec();
        let zs = b.fancy_addition(&xs);
        b.outputs(&zs);
        let circ = b.finish();

        let (gb, ev) = garble(&circ);
        println!("mods={:?} nargs={} size={}", mods, nargs, ev.size());

        let Q: u128 = mods.iter().map(|&q| q as u128).product();

        // test random values
        for _ in 0..64 {
            let mut should_be = 0;
            let mut ds = Vec::new();
            for _ in 0..nargs {
                let x = rng.gen_u128() % Q;
                should_be = (should_be + x) % Q;
                ds.extend(numbers::as_mixed_radix(x, &mods).iter());
            }
            let X = gb.encode(&ds);
            let Y = ev.eval(&circ, &X);
            let res = gb.decode(&Y);
            assert_eq!(numbers::from_mixed_radix(&res,&mods), should_be);
        }
    }
//}}}
    #[test] // constants {{{
    fn constants() {
        let mut b = Builder::new();
        let mut rng = Rng::new();

        let q = rng.gen_modulus();
        let c = rng.gen_u16() % q;

        let x = b.input(q);
        let y = b.constant(c,q);
        let z = b.add(x,y);
        b.output(z);

        let circ = b.finish();
        let (gb, ev) = garble(&circ);

        for _ in 0..64 {
            let x = rng.gen_u16() % q;

            assert_eq!(circ.eval(&[x])[0], (x+c)%q, "plaintext");

            let X = gb.encode(&[x]);
            let Y = ev.eval(&circ, &X);
            assert_eq!(gb.decode(&Y)[0], (x+c)%q, "garbled");
        }
    }
//}}}
}
