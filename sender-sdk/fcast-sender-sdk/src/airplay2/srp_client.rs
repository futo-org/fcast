use num_bigint::BigUint;
use rand_pcg::rand_core::RngCore;
use sha2::{Digest, Sha512};

#[derive(Debug, thiserror::Error)]
pub enum SrpError {
    #[error("Invalid client parameter: A mod N = 0")]
    InvalidClientParameter,
    #[error("Missing A")]
    MissingBigA,
    #[error("Missing a")]
    MissingA,
    #[error("Invalid server parameter: u = 0")]
    InvalidServerParameter,
    #[error("Invalid server parameter: B mod N = 0")]
    InvalidServerParameterB,
    #[error("Must call srp_user_process_challenge() first")]
    MustProcessChallengeFirst,
}

pub struct SrpClient {
    n: BigUint,
    g: BigUint,
    big_a: Option<BigUint>,
    a: Option<BigUint>,
    username: Vec<u8>,
    password: Vec<u8>,
    big_s: Option<BigUint>,
    key: Option<Vec<u8>>,
    m1: Option<Vec<u8>>,
}

// TODO: use the digest's GenericArray whenever possible to eliminate temporary allocs.
impl SrpClient {
    pub fn new(n: BigUint, g: BigUint, username: Vec<u8>, password: Vec<u8>) -> Self {
        Self {
            n,
            g,
            big_a: None,
            a: None,
            username,
            password,
            big_s: None,
            key: None,
            m1: None,
        }
    }

    #[cfg(test)]
    pub fn get_big_s(&self) -> Option<BigUint> {
        self.big_s.clone()
    }

    pub fn get_session_key(&self) -> Option<Vec<u8>> {
        self.key.clone()
    }

    pub fn srp_user_start_authentication(
        &mut self,
        a_override: Option<BigUint>,
    ) -> Result<BigUint, SrpError> {
        let mut rng = rand_pcg::Pcg64::new(0xcafef00dd15ea5e5, 0xa02bdbf7bb3c0a7);
        let a = a_override.unwrap_or_else(|| {
            let mut a_buf = [0u8; 256];
            rng.fill_bytes(&mut a_buf);
            BigUint::from_bytes_be(&a_buf)
        });
        let big_a = self.g.modpow(&a, &self.n);
        if big_a == BigUint::ZERO {
            return Err(SrpError::InvalidClientParameter);
        }
        self.a = Some(a);
        self.big_a = Some(big_a.clone());
        Ok(big_a)
    }

    fn h_padded(&self, bn1: &BigUint, bn2: &BigUint) -> BigUint {
        fn pad_to(value: &BigUint, length: u64) -> Vec<u8> {
            let length = length as usize;
            let minimal = value.to_bytes_be();
            if minimal.len() == length {
                minimal
            } else if minimal.len() < length {
                let mut res = vec![0u8; length - minimal.len()];
                res.extend_from_slice(&minimal);
                res
            } else {
                minimal[minimal.len() - length..minimal.len()].to_vec()
            }
        }

        let pad_l = self.n.bits().div_ceil(8);

        let mut d = Sha512::new();
        d.update(pad_to(bn1, pad_l));
        d.update(pad_to(bn2, pad_l));
        BigUint::from_bytes_be(&d.finalize())
    }

    fn h_nn(&self, bn1: &BigUint, bn2: &BigUint) -> BigUint {
        self.h_padded(bn1, bn2)
    }

    fn h_ns(n: &[u8], salt: &[u8]) -> BigUint {
        let mut d = Sha512::new();
        d.update(n);
        d.update(salt);
        BigUint::from_bytes_be(&d.finalize())
    }

    fn compute_x(&self, salt: BigUint) -> BigUint {
        let mut user_colon_pass = self.username.clone();
        user_colon_pass.push(b':');
        user_colon_pass.extend_from_slice(&self.password);
        let ucp_hash = Sha512::digest(&user_colon_pass).to_vec();
        Self::h_ns(&salt.to_bytes_be(), &ucp_hash)
    }

    fn compute_m1(
        &self,
        salt_bytes: &[u8],
        big_a_int: &BigUint,
        big_b_int: &BigUint,
        big_k: &[u8],
    ) -> Vec<u8> {
        let n_bytes = self.n.to_bytes_be();
        let g_bytes = self.g.to_bytes_be();
        let h_n = Sha512::digest(&n_bytes);
        let h_g = Sha512::digest(&g_bytes);
        let mut h_xor = vec![0u8; h_n.len()];
        for i in 0..h_n.len() {
            h_xor[i] = h_n[i] ^ h_g[i];
        }
        let h_i = Sha512::digest(&self.username);
        let mut d = Sha512::new();
        d.update(h_xor);
        d.update(h_i);
        d.update(salt_bytes);
        d.update(big_a_int.to_bytes_be());
        d.update(big_b_int.to_bytes_be());
        d.update(big_k);
        d.finalize().to_vec()
    }

    pub fn user_process_challenge_internal(
        &mut self,
        salt_bytes: &[u8],
        big_b_bytes: &[u8],
    ) -> Result<(BigUint, BigUint, Vec<u8>), SrpError> {
        let Some(big_a) = self.big_a.as_ref() else {
            return Err(SrpError::MissingBigA);
        };
        let Some(a) = self.a.as_ref() else {
            return Err(SrpError::MissingA);
        };

        let big_b = BigUint::from_bytes_be(big_b_bytes);
        if big_b == BigUint::ZERO {
            return Err(SrpError::InvalidServerParameterB);
        }
        let u = self.h_nn(big_a, &big_b);
        if u == BigUint::ZERO {
            return Err(SrpError::InvalidServerParameter);
        }

        let x = self.compute_x(BigUint::from_bytes_be(salt_bytes));
        let k = self.h_nn(&self.n, &self.g);
        let v = self.g.modpow(&x, &self.n);
        let kv = (k * &v) % &self.n;
        let base = ((&self.n + &big_b) - kv) % &self.n;
        let exponent = (&u * x) + a;
        let big_s = base.modpow(&exponent, &self.n);

        let session_key = Sha512::digest(big_s.to_bytes_be()).to_vec();
        let m1 = self.compute_m1(salt_bytes, big_a, &big_b, &session_key);

        let res = (u, v, m1.clone());

        self.key = Some(session_key);
        self.big_s = Some(big_s);
        self.m1 = Some(m1);

        Ok(res)
    }

    pub fn srp_user_process_challenge(
        &mut self,
        salt_bytes: &[u8],
        big_b_bytes: &[u8],
    ) -> Result<Vec<u8>, SrpError> {
        Ok(self
            .user_process_challenge_internal(salt_bytes, big_b_bytes)?
            .2)
    }

    fn compute_m2(big_a_int: &BigUint, big_m: &[u8], big_k: &[u8]) -> Vec<u8> {
        let mut d = Sha512::new();
        d.update(big_a_int.to_bytes_be());
        d.update(big_m);
        d.update(big_k);
        d.finalize().to_vec()
    }

    pub fn user_verify_session(&self, server_m2: &[u8]) -> Result<bool, SrpError> {
        let Some(big_m) = self.m1.as_ref() else {
            return Err(SrpError::MustProcessChallengeFirst);
        };
        let Some(session_key) = self.key.as_ref() else {
            return Err(SrpError::MustProcessChallengeFirst);
        };
        let Some(big_a) = self.big_a.as_ref() else {
            return Err(SrpError::MustProcessChallengeFirst);
        };

        let m2 = Self::compute_m2(big_a, big_m, session_key);
        Ok(m2 == server_m2)
    }
}
