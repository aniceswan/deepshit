//! PoW DeepSeekHashV1: SHA3-256 dengan Keccak-f[1600] rounds 1..23.
//!
//! Protokol (hasil riset F0):
//! - server: pilih answer in [0, difficulty), hitung
//!   `challenge = DeepSeekHashV1(prefix + str(answer))`, prefix = "{salt}_{expire_at}_"
//! - klien: brute-force nonce sampai hash == challenge.
//! - header: base64(JSON{algorithm, challenge, salt, answer, signature, target_path})
//!   dikirim sebagai `x-ds-pow-response`.
//!
//! Referensi: redeflesq/deepseek-pow, Brhiza/ds2api, dyunwei/ds2api.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};

use crate::error::DsError;

// --- Keccak-f[1600] ---------------------------------------------------------

const RC: [u64; 24] = [
    0x0000000000000001,
    0x0000000000008082,
    0x800000000000808a,
    0x8000000080008000,
    0x000000000000808b,
    0x0000000080000001,
    0x8000000080008081,
    0x8000000000008009,
    0x000000000000008a,
    0x0000000000000088,
    0x0000000080008009,
    0x000000008000000a,
    0x000000008000808b,
    0x800000000000008b,
    0x8000000000008089,
    0x8000000000008003,
    0x8000000000008002,
    0x8000000000000080,
    0x000000000000800a,
    0x800000008000000a,
    0x8000000080008081,
    0x8000000000008080,
    0x0000000080000001,
    0x8000000080008008,
];

/// Offset rotasi r[x][y] (FIPS 202). Indeks pertama = x, kedua = y.
const RHO: [[u32; 5]; 5] = [
    [0, 36, 3, 41, 18],
    [1, 44, 10, 45, 2],
    [62, 6, 43, 15, 61],
    [28, 55, 25, 21, 56],
    [27, 20, 39, 8, 14],
];

/// Keccak-f[1600] rounds 1..23 (lewati round 0) — inti DeepSeekHashV1.
fn keccak_f23(state: &mut [u64; 25]) {
    for &rc in RC.iter().skip(1) {
        let mut c = [0u64; 5];
        for x in 0..5 {
            c[x] = state[x] ^ state[x + 5] ^ state[x + 10] ^ state[x + 15] ^ state[x + 20];
        }
        let mut d = [0u64; 5];
        for x in 0..5 {
            d[x] = c[(x + 4) % 5] ^ c[(x + 1) % 5].rotate_left(1);
        }
        for x in 0..5 {
            for y in 0..5 {
                state[x + 5 * y] ^= d[x];
            }
        }
        // Rho + Pi: B[y, (2x+3y)%5] = ROT(A[x,y], r[x][y])
        let mut b = [0u64; 25];
        for x in 0..5 {
            for y in 0..5 {
                let nx = y;
                let ny = (2 * x + 3 * y) % 5;
                b[nx + 5 * ny] = state[x + 5 * y].rotate_left(RHO[x][y]);
            }
        }
        // Chi
        for y in 0..5 {
            for x in 0..5 {
                state[x + 5 * y] =
                    b[x + 5 * y] ^ ((!b[(x + 1) % 5 + 5 * y]) & b[(x + 2) % 5 + 5 * y]);
            }
        }
        // Iota
        state[0] ^= rc;
    }
}

// --- SHA3-256 (rate 136) dengan permutasi kustom ---

const RATE: usize = 136;

pub fn deepseek_hash_v1(data: &[u8]) -> [u8; 32] {
    let mut st = [0u64; 25];
    let mut off = 0;
    while off + RATE <= data.len() {
        for i in 0..(RATE / 8) {
            let mut w = [0u8; 8];
            w.copy_from_slice(&data[off + i * 8..off + i * 8 + 8]);
            st[i] ^= u64::from_le_bytes(w);
        }
        keccak_f23(&mut st);
        off += RATE;
    }
    // Blok terakhir + padding SHA3 (0x06 ... 0x80)
    let mut block = [0u8; RATE];
    let rem = data.len() - off;
    block[..rem].copy_from_slice(&data[off..]);
    block[rem] ^= 0x06;
    block[RATE - 1] ^= 0x80;
    for i in 0..(RATE / 8) {
        let mut w = [0u8; 8];
        w.copy_from_slice(&block[i * 8..i * 8 + 8]);
        st[i] ^= u64::from_le_bytes(w);
    }
    keccak_f23(&mut st);
    let mut out = [0u8; 32];
    for i in 0..4 {
        out[i * 8..i * 8 + 8].copy_from_slice(&st[i].to_le_bytes());
    }
    out
}

pub fn deepseek_hash_hex(data: &[u8]) -> String {
    hex_of(&deepseek_hash_v1(data))
}

fn hex_of(b: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(H[(x >> 4) as usize] as char);
        s.push(H[(x & 0xf) as usize] as char);
    }
    s
}

fn hex_to_32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        let hi = s.as_bytes()[2 * i] as char;
        let lo = s.as_bytes()[2 * i + 1] as char;
        let h = hi.to_digit(16)? as u8;
        let l = lo.to_digit(16)? as u8;
        out[i] = (h << 4) | l;
    }
    Some(out)
}

// --- Solver ---

use crate::types::Challenge;

pub fn build_prefix(salt: &str, expire_at: i64) -> String {
    format!("{salt}_{expire_at}_")
}

/// Cari nonce n in [0, difficulty) dengan hash(prefix + str(n)) == challenge.
/// Mengembalikan None bila tidak ketemu (challenge kedaluwarsa / salah).
pub fn solve_pow(challenge_hex: &str, salt: &str, expire_at: i64, difficulty: u64) -> Option<u64> {
    let target = hex_to_32(challenge_hex.trim())?;
    let prefix = build_prefix(salt, expire_at);
    let pre = prefix.as_bytes();
    // Buffer kerja: prefix + nonce desimal (maks 20 digit).
    let mut buf = Vec::with_capacity(pre.len() + 20);
    for n in 0..difficulty {
        buf.clear();
        buf.extend_from_slice(pre);
        buf.extend_from_slice(n.to_string().as_bytes());
        if deepseek_hash_v1(&buf) == target {
            return Some(n);
        }
    }
    None
}

/// Bentuk header `x-ds-pow-response`: base64(JSON tanpa difficulty/expire_at).
pub fn build_pow_header(ch: &Challenge, answer: u64) -> String {
    let payload = serde_json::json!({
        "algorithm": ch.algorithm,
        "challenge": ch.challenge,
        "salt": ch.salt,
        "answer": answer,
        "signature": ch.signature,
        "target_path": ch.target_path,
    });
    let raw = serde_json::to_vec(&payload).unwrap_or_default();
    B64.encode(raw)
}

/// Ambil challenge -> pecahkan -> header. Error bila algorithm asing / tak ketemu.
pub fn solve_and_build_header(ch: &Challenge) -> Result<String, DsError> {
    if ch.algorithm != "DeepSeekHashV1" {
        return Err(DsError::Pow(format!("unknown algorithm: {}", ch.algorithm)));
    }
    let ans = solve_pow(&ch.challenge, &ch.salt, ch.expire_at, ch.difficulty)
        .ok_or_else(|| DsError::Pow("no nonce found within difficulty".into()))?;
    Ok(build_pow_header(ch, ans))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_vectors_wasm() {
        // Vektor dari pemanggilan WASM resmi (Brhiza/ds2api).
        let cases = [
            ("", "e594808bc5b7151ac160c6d39a02e0a8e261ed588578403099e3561dc40c26b3"),
            (
                "testsalt_1700000000_42",
                "d4a2ea58c89e40887c933484868380c6f803eaa8dc53a3b9df8e431b921a4f09",
            ),
            (
                "testsalt_1700000000_100000",
                "abea2f35796b65486e9be1b36f7878c66cab021e96faa473fdf4decd31f9ba30",
            ),
            (
                "abc123salt_1700000000_12345",
                "74b3b7452745b70e85eb32ee7f0a9ec0381d42dd5137b695da915e104fc390e1",
            ),
        ];
        for (inp, want) in cases {
            assert_eq!(deepseek_hash_hex(inp.as_bytes()), want, "input {inp:?}");
        }
    }

    #[test]
    fn solve_small() {
        // Buat challenge sintetis lalu pastikan solver menemukan jawabannya.
        for ans in [0u64, 1, 42, 500] {
            let salt = "testsalt";
            let exp = 1700000000i64;
            let h = deepseek_hash_hex(format!("{salt}_{exp}_{ans}").as_bytes());
            let got = solve_pow(&h, salt, exp, 2000).expect("must be found");
            assert_eq!(got, ans);
        }
    }

    #[test]
    fn header_roundtrip() {
        let salt = "salt";
        let exp = 1712345678i64;
        let ans = 777u64;
        let h = deepseek_hash_hex(format!("{salt}_{exp}_{ans}").as_bytes());
        let ch = Challenge {
            algorithm: "DeepSeekHashV1".into(),
            challenge: h,
            salt: salt.into(),
            signature: "sig".into(),
            difficulty: 2000,
            expire_at: exp,
            target_path: "/api/v0/chat/completion".into(),
        };
        let hdr = solve_and_build_header(&ch).unwrap();
        let raw = B64.decode(&hdr).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["answer"], 777);
        assert_eq!(v["salt"], "salt");
    }

    #[test]
    fn solve_miss_returns_none() {
        let got = solve_pow(
            "0000000000000000000000000000000000000000000000000000000000000000",
            "x",
            1,
            50,
        );
        assert!(got.is_none());
    }
}
