use circuit::{Circuit, Gate, Ref};
use rand::Rng;
use wire::Wire;
use util::IterToVec;

use std::collections::HashMap;

use scoped_pool::Pool;

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
    let n = c.gates.len();

    let mut wires: Vec<Wire> = vec![Wire::zero(2);n];
    let mut dependents: Vec<Vec<Ref>> = vec![Vec::new();n];
    let mut depends_on: Vec<Ref>      = vec![0;n];

    let mut init = Vec::new();

    for i in 0..n {
        gb.create_delta(c.modulus(i));
        match c.gates[i] {
            Gate::Cmul { xref, .. }  => { dependents[xref].push(i); depends_on[i] = 1 }
            Gate::Proj { xref, .. }  => { dependents[xref].push(i); depends_on[i] = 1 }
            Gate::Add { xref, yref } => { dependents[xref].push(i); dependents[yref].push(i); depends_on[i] = 2 }
            Gate::Sub { xref, yref } => { dependents[xref].push(i); dependents[yref].push(i); depends_on[i] = 2 }
            Gate::Yao      { xref, yref, .. } => { dependents[xref].push(i); dependents[yref].push(i); depends_on[i] = 2 }
            Gate::HalfGate { xref, yref, .. } => { dependents[xref].push(i); dependents[yref].push(i); depends_on[i] = 2 }
            Gate::Input { .. } => { init.push(i); wires[i] = gb.input(c.modulus(i)) }
            Gate::Const { .. } => { init.push(i); wires[i] = gb.constant(c.modulus(i)) }
        }
    }

    let pool = Pool::new(num_cpus::get());

    let (tx, rx) = std::sync::mpsc::channel::<(Ref,Wire,Option<(Ref,GarbledGate)>)>();

    let mut gates: Vec<Option<GarbledGate>> = vec![None;n];

    let mut n_outputs_completed = 0;

    for i in init.into_iter() {
        tx.send((i, wires[i].clone(), None)).unwrap();
    }

    loop {
        let (i, wire, maybeGate) = rx.recv().unwrap();

        wires[i] = wire;

        if let Some((id, gate)) = maybeGate {
            gates[id] = Some(gate);
        }

        if c.output_refs.contains(&i) {
            n_outputs_completed += 1;
            if n_outputs_completed == c.noutputs() {
                break;
            }
        }

        for &dep in dependents[i].iter() {
            depends_on[dep] -= 1;
            if depends_on[dep] == 0 {
                // schedule dep
                let tx = tx.clone();
                let c = &c;
                let wires = &wires;
                let gb = &gb;
                pool.scoped(|scope| {
                    scope.execute(move || {
                        let q = c.modulus(dep);
                        let res = match c.gates[dep] {
                            Gate::Add { xref, yref } => (dep, (wires[xref]).plus(&wires[yref]), None),
                            Gate::Sub { xref, yref } => (dep, (wires[xref]).minus(&wires[yref]), None),
                            Gate::Cmul { xref, c }   => (dep, (wires[xref]).cmul(c), None),

                            Gate::Proj { xref, ref tt, id }  => {
                                let X = &wires[xref];
                                let (w,g) = gb.proj(X, q, tt, i);
                                (dep, w, Some((id,g)))
                            }

                            Gate::Yao { xref, yref, ref tt, id } => {
                                let X = &wires[xref];
                                let Y = &wires[yref];
                                let (w,g) = gb.yao(X, Y, q, tt, i);
                                (dep, w, Some((id,g)))
                            }

                            Gate::HalfGate { xref, yref, id }  => {
                                let X = &wires[xref];
                                let Y = &wires[yref];
                                let (w,g) = gb.half_gate(X, Y, q, i);
                                (dep, w, Some((id, g)))
                            }
                            _ => unimplemented!(),
                        };
                        tx.send(res).unwrap();
                    });
                });
            }
        }
    }

    for (i,&r) in c.output_refs.iter().enumerate() {
        gb.output(&wires[r], i);
    }

    let gates = gates.into_iter().take_while(|g| g.is_some()).map(|g| g.unwrap()).to_vec();

    let cs = c.const_vals.as_ref().expect("constants needed!");
    let ev = Evaluator::new(gates, gb.encode_consts(cs));
    (gb, ev)
}

pub fn sync_garble(c: &Circuit) -> (Garbler, Evaluator) {
    let mut gb = Garbler::new();

    let mut wires: Vec<Wire> = Vec::new();
    let mut gates: Vec<GarbledGate> = Vec::new();

    for i in 0..c.gates.len() {
        let q = c.moduli[i];
        gb.create_delta(q);
        let w = match c.gates[i] {
            Gate::Input { .. } => gb.input(q),
            Gate::Const { .. } => gb.constant(q),

            Gate::Add { xref, yref } => wires[xref].plus(&wires[yref]),
            Gate::Sub { xref, yref } => wires[xref].minus(&wires[yref]),
            Gate::Cmul { xref, c }   => wires[xref].cmul(c),

            Gate::Proj { xref, ref tt, .. }  => {
                let X = wires[xref].clone();
                let (w,g) = gb.proj(&X, q, tt, i);
                gates.push(g);
                w
            }

            Gate::Yao { xref, yref, ref tt, .. } => {
                let X = wires[xref].clone();
                let Y = wires[yref].clone();
                let (w,g) = gb.yao(&X, &Y, q, tt, i);
                gates.push(g);
                w
            }

            Gate::HalfGate { xref, yref, .. }  => {
                let X = wires[xref].clone();
                let Y = wires[yref].clone();
                let (w,g) = gb.half_gate(&X, &Y, q, i);
                gates.push(g);
                w
            }
        };
        wires.push(w); // add the new zero-wire
    }

    for (i,&r) in c.output_refs.iter().enumerate() {
        gb.output(&wires[r], i);
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

    fn delta(&self, q: u16) -> &Wire {
        self.deltas.get(&q).expect(&format!("delta not created for {}!", q))
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
                 .negate()
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
                 .negate()
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

    pub fn half_gate(&self, A: &Wire, B: &Wire, q: u16, gate_num: usize)
        -> (Wire, GarbledGate)
    {
        let mut gate = vec![None; 2 * q as usize - 2];
        let g = tweak(gate_num);

        let r = B.color(); // secret value known only to the garbler (ev knows r+b)

        let D = &self.delta(q); // delta for this modulus

        // X = -H(A+aD) - arD such that a + A.color == 0
        let alpha = q - A.color(); // alpha = -A.color
        let X = A.plus(&D.cmul(alpha)).hashback(g,q).negate()
                 .plus(&D.cmul(alpha * r % q));

        // Y = -H(B + bD) + brA
        let beta = q - B.color();
        let Y = B.plus(&D.cmul(beta)).hashback(g,q).negate()
                 .plus(&A.cmul((beta + r) % q));

        for i in 0..q {
            // garbler's half-gate: outputs X-arD
            // G = H(A+aD) + X+a(-r)D = H(A+aD) + X-arD
            let a = i; // a: truth value of wire X
            let A_ = A.plus(&self.delta(q).cmul(a));
            if A_.color() != 0 {
                let tao = a * (q - r) % q;
                let G = A_.hash(g) ^ X.plus(&D.cmul(tao)).as_u128();
                gate[A_.color() as usize - 1] = Some(G);
            }

            // evaluator's half-gate: outputs Y+a(r+b)D
            // G = H(B+bD) + Y-(b+r)A
            let B_ = B.plus(&D.cmul(i));
            if B_.color() != 0 {
                let G = B_.hash(g) ^ Y.minus(&A.cmul((i+r)%q)).as_u128();
                gate[(q + B_.color()) as usize - 2] = Some(G);
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
        debug_assert_eq!(ws.len(), outs.len());
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
            let q = c.moduli[i];
            let w = match c.gates[i] {

                Gate::Input { id }       => inputs[id].clone(),
                Gate::Const { id, .. }   => self.consts[id].clone(),
                Gate::Add { xref, yref } => wires[xref].plus(&wires[yref]),
                Gate::Sub { xref, yref } => wires[xref].minus(&wires[yref]),
                Gate::Cmul { xref, c }   => wires[xref].cmul(c),

                Gate::Proj { xref, id, .. } => {
                    let x = &wires[xref];
                    if x.color() == 0 {
                        x.hashback(i as u128, q).negate()
                    } else {
                        let ct = self.gates[id][x.color() as usize - 1];
                        Wire::from_u128(ct ^ x.hash(i as u128), q)
                    }
                }

                Gate::Yao { xref, yref, id, .. } => {
                    let a = &wires[xref];
                    let b = &wires[yref];
                    if a.color() == 0 && b.color() == 0 {
                        a.hashback2(&b, tweak(i), q).negate()
                    } else {
                        let ix = a.color() as usize * c.moduli[yref] as usize + b.color() as usize;
                        let ct = self.gates[id][ix - 1];
                        Wire::from_u128(ct ^ a.hash2(&b, tweak(i)), q)
                    }
                }

                Gate::HalfGate { xref, yref, id } => {
                    let g = tweak(i);

                    // garbler's half gate
                    let A = &wires[xref];
                    let L = if A.color() == 0 {
                        A.hashback(g,q).negate()
                    } else {
                        let ct_left = self.gates[id][A.color() as usize - 1];
                        Wire::from_u128(ct_left ^ A.hash(g), q)
                    };

                    // evaluator's half gate
                    let B = &wires[yref];
                    let R = if B.color() == 0 {
                        B.hashback(g,q).negate()
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
                let inps = &(0..c.ninputs()).map(|i| {
                    rng.gen_u16() % c.input_mod(i)
                }).collect::<Vec<u16>>();
                let xs = &gb.encode(inps);
                let ys = &ev.eval(c, xs);
                assert_eq!(gb.decode(ys)[0], c.eval(inps)[0], "q={}", q);
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
    #[test] // mul_dlog {{{
    fn mul_dlog() {
        garble_test_helper(|q| {
            let mut b = Builder::new();
            let x = b.input(q);
            let y = b.input(q);
            let z = b.mul_dlog(&[x,y]);
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
