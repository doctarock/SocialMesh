//! Reed–Solomon error correction for one modem frame: RS(255, 239) over
//! GF(2^8) (primitive polynomial 0x11d, generator roots α^0 … α^15),
//! shortened to the frame's length. [`PARITY`] check bytes repair up to 8
//! wrong bytes anywhere in the frame.
//!
//! Sound loses whole frames (handled by the datagram layer's fountain code)
//! but it also flips single symbols: a weak tone beaten by the echo of the
//! one before. Without this a single flipped symbol wastes a 4-second frame.

/// Check bytes per frame.
pub const PARITY: usize = 16;
/// Largest frame: the code is at most 255 bytes long.
pub const MAX_DATA: usize = 255 - PARITY;

const fn tables() -> ([u8; 512], [u8; 256]) {
    let mut exp = [0u8; 512];
    let mut log = [0u8; 256];
    let mut x: u16 = 1;
    let mut i = 0;
    while i < 255 {
        exp[i] = x as u8;
        log[x as usize] = i as u8;
        x <<= 1;
        if x & 0x100 != 0 {
            x ^= 0x11d;
        }
        i += 1;
    }
    while i < 512 {
        exp[i] = exp[i - 255];
        i += 1;
    }
    (exp, log)
}

const TABLES: ([u8; 512], [u8; 256]) = tables();
const EXP: [u8; 512] = TABLES.0;
const LOG: [u8; 256] = TABLES.1;

fn mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        0
    } else {
        EXP[LOG[a as usize] as usize + LOG[b as usize] as usize]
    }
}

fn div(a: u8, b: u8) -> u8 {
    debug_assert!(b != 0);
    if a == 0 {
        0
    } else {
        EXP[(LOG[a as usize] as usize + 255 - LOG[b as usize] as usize) % 255]
    }
}

/// α^e for any (possibly negative) exponent.
fn pow_a(e: i32) -> u8 {
    EXP[e.rem_euclid(255) as usize]
}

/// Generator polynomial, highest power first: Π (x − α^i), i < PARITY.
fn generator() -> [u8; PARITY + 1] {
    let mut g = [0u8; PARITY + 1];
    g[0] = 1;
    for i in 0..PARITY {
        // Multiply by (x + α^i): g has degree i before this step.
        let r = pow_a(i as i32);
        for j in (1..=i + 1).rev() {
            g[j] ^= mul(g[j - 1], r);
        }
    }
    g
}

/// `data` followed by its check bytes. `data` is at most [`MAX_DATA`] bytes.
pub fn encode(data: &[u8]) -> Vec<u8> {
    assert!(data.len() <= MAX_DATA);
    let g = generator();
    let mut par = [0u8; PARITY];
    for &m in data {
        let fb = m ^ par[0];
        par.copy_within(1.., 0);
        par[PARITY - 1] = 0;
        if fb != 0 {
            for i in 0..PARITY {
                par[i] ^= mul(fb, g[i + 1]);
            }
        }
    }
    let mut out = data.to_vec();
    out.extend_from_slice(&par);
    out
}

/// Repair `code` (data then check bytes) in place. Returns the number of
/// bytes corrected, or None if there were too many errors to fix.
pub fn decode(code: &mut [u8]) -> Option<usize> {
    let n = code.len();
    if !(PARITY + 1..=255).contains(&n) {
        return None;
    }
    let syndromes = |c: &[u8]| -> [u8; PARITY] {
        std::array::from_fn(|j| {
            let a = pow_a(j as i32);
            c.iter().fold(0u8, |acc, &v| mul(acc, a) ^ v)
        })
    };
    let s = syndromes(code);
    if s.iter().all(|&v| v == 0) {
        return Some(0);
    }

    // Berlekamp–Massey: error locator Λ(x), lowest power first.
    let mut lambda = vec![0u8; PARITY + 1];
    lambda[0] = 1;
    let mut b = lambda.clone();
    let (mut l, mut m, mut bb) = (0usize, 1usize, 1u8);
    for k in 0..PARITY {
        let mut d = s[k];
        for i in 1..=l {
            d ^= mul(lambda[i], s[k - i]);
        }
        if d == 0 {
            m += 1;
            continue;
        }
        let coef = div(d, bb);
        let prev = lambda.clone();
        for i in 0..=PARITY - m {
            lambda[i + m] ^= mul(coef, b[i]);
        }
        if 2 * l <= k {
            l = k + 1 - l;
            b = prev;
            bb = d;
            m = 1;
        } else {
            m += 1;
        }
    }
    if l == 0 || l > PARITY / 2 {
        return None;
    }
    let eval = |poly: &[u8], x: u8| poly.iter().rev().fold(0u8, |acc, &c| mul(acc, x) ^ c);

    // Chien search: position i holds the coefficient of x^(n-1-i).
    let mut positions = Vec::new();
    for i in 0..n {
        let p = (n - 1 - i) as i32;
        if eval(&lambda[..=l], pow_a(-p)) == 0 {
            positions.push(i);
        }
    }
    if positions.len() != l {
        return None;
    }

    // Forney: Ω(x) = S(x)Λ(x) mod x^PARITY; e = X·Ω(X⁻¹)/Λ'(X⁻¹).
    let mut omega = [0u8; PARITY];
    for (i, &si) in s.iter().enumerate() {
        for (j, &lj) in lambda[..=l].iter().enumerate() {
            if i + j < PARITY {
                omega[i + j] ^= mul(si, lj);
            }
        }
    }
    // Formal derivative: only odd powers survive in characteristic 2.
    let deriv: Vec<u8> = (1..=l).map(|i| if i % 2 == 1 { lambda[i] } else { 0 }).collect();
    for &i in &positions {
        let p = (n - 1 - i) as i32;
        let (x, x_inv) = (pow_a(p), pow_a(-p));
        let den = eval(&deriv, x_inv);
        if den == 0 {
            return None;
        }
        code[i] ^= mul(x, div(eval(&omega, x_inv), den));
    }
    // A correct repair leaves no syndrome; anything else was beyond the code.
    syndromes(code).iter().all(|&v| v == 0).then_some(l)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rngs::StdRng, Rng, SeedableRng};

    #[test]
    fn field_tables_are_consistent() {
        for a in 1..=255u8 {
            assert_eq!(mul(a, div(1, a)), 1);
        }
        assert_eq!(mul(0x53, 0xca), mul(0xca, 0x53));
    }

    #[test]
    fn clean_codewords_pass_and_check_bytes_are_appended() {
        let data = b"socialmesh";
        let mut c = encode(data);
        assert_eq!(c.len(), data.len() + PARITY);
        assert_eq!(&c[..data.len()], data);
        assert_eq!(decode(&mut c), Some(0));
    }

    #[test]
    fn repairs_up_to_eight_errors_anywhere() {
        let mut r = StdRng::seed_from_u64(1);
        for trial in 0..400 {
            let len = r.gen_range(1..=MAX_DATA);
            let data: Vec<u8> = (0..len).map(|_| r.gen()).collect();
            let clean = encode(&data);
            let mut c = clean.clone();
            let errs = trial % (PARITY / 2 + 1);
            let mut hit = std::collections::HashSet::new();
            while hit.len() < errs {
                hit.insert(r.gen_range(0..c.len()));
            }
            for &i in &hit {
                c[i] ^= r.gen_range(1..=255u8);
            }
            assert_eq!(decode(&mut c), Some(errs), "len {len}, {errs} errors");
            assert_eq!(c, clean);
        }
    }

    #[test]
    fn too_many_errors_are_not_silently_accepted_as_the_original() {
        let mut r = StdRng::seed_from_u64(2);
        for _ in 0..200 {
            let data: Vec<u8> = (0..84).map(|_| r.gen()).collect();
            let clean = encode(&data);
            let mut c = clean.clone();
            for i in 0..12 {
                c[i * 7] ^= r.gen_range(1..=255u8);
            }
            // Either refused, or (rarely) "repaired" into some other codeword;
            // never the original. The datagram checksum catches the latter.
            if decode(&mut c).is_some() {
                assert_ne!(c, clean);
            }
        }
    }
}
