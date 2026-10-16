//! Client side of SASL `SCRAM-SHA-256` (RFC 5802 / RFC 7677), without channel
//! binding (`n,,`). Ported from arkhe's `SCRAMClient`. The password never
//! crosses the wire, and the server has to prove it knows the stored key too:
//! a wrong server signature aborts the login.
//!
//! Flow: [`ScramClient::client_first`] → feed the server-first message to
//! [`ScramClient::client_final`] → check the server-final message with
//! [`ScramClient::verify`].

use std::num::NonZeroU32;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ring::{digest, hmac, pbkdf2};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Initial,
    SentFirst,
    SentFinal,
}

#[derive(Debug)]
pub struct ScramClient {
    pub step: Step,
    username: String,
    password: String,
    nonce: String,
    /// `(ServerKey, AuthMessage)`: what the server signature must be an
    /// HMAC of, once the client-final message has been built.
    server_proof_input: Option<(Vec<u8>, String)>,
}

impl ScramClient {
    pub fn new(username: &str, password: &str) -> Self {
        Self::with_nonce(username, password, random_nonce())
    }

    /// `nonce` is injectable for test vectors.
    pub fn with_nonce(username: &str, password: &str, nonce: String) -> Self {
        ScramClient {
            step: Step::Initial,
            username: username.to_string(),
            password: password.to_string(),
            nonce,
            server_proof_input: None,
        }
    }

    fn client_first_bare(&self) -> String {
        format!("n={},r={}", escape(&self.username), self.nonce)
    }

    /// `n,,n=<user>,r=<nonce>`: the first message we send.
    pub fn client_first(&mut self) -> String {
        self.step = Step::SentFirst;
        format!("n,,{}", self.client_first_bare())
    }

    /// The client-final message (with proof) for the server-first message, or
    /// `None` if it is malformed, carries a mandatory extension, or its nonce
    /// doesn't extend ours.
    pub fn client_final(&mut self, server_first: &str) -> Option<String> {
        if self.step != Step::SentFirst {
            return None;
        }
        let attr = |k: &str| attribute(server_first, k);
        if attr("m").is_some() {
            return None;
        }
        let nonce = attr("r")?;
        if !nonce.starts_with(&self.nonce) || nonce.len() <= self.nonce.len() {
            return None;
        }
        let salt = B64.decode(attr("s")?).ok()?;
        let iterations = NonZeroU32::new(attr("i")?.parse().ok()?)?;

        let mut salted = [0u8; digest::SHA256_OUTPUT_LEN];
        pbkdf2::derive(
            pbkdf2::PBKDF2_HMAC_SHA256,
            iterations,
            &salt,
            self.password.as_bytes(),
            &mut salted,
        );
        let client_key = hmac_sha256(&salted, b"Client Key");
        let stored_key = digest::digest(&digest::SHA256, &client_key);
        let final_without_proof = format!("c=biws,r={nonce}");
        let auth_message =
            format!("{},{server_first},{final_without_proof}", self.client_first_bare());
        let client_signature = hmac_sha256(stored_key.as_ref(), auth_message.as_bytes());
        let proof: Vec<u8> = client_key.iter().zip(&client_signature).map(|(a, b)| a ^ b).collect();
        let server_key = hmac_sha256(&salted, b"Server Key");
        self.server_proof_input = Some((server_key, auth_message));
        self.step = Step::SentFinal;
        Some(format!("{final_without_proof},p={}", B64.encode(proof)))
    }

    /// Whether the server-final message carries the signature we expect.
    pub fn verify(&self, server_final: &str) -> bool {
        if self.step != Step::SentFinal {
            return false;
        }
        let (Some((server_key, auth_message)), Some(sig)) =
            (&self.server_proof_input, attribute(server_final, "v"))
        else {
            return false;
        };
        let key = hmac::Key::new(hmac::HMAC_SHA256, server_key);
        // Constant-time comparison against HMAC(ServerKey, AuthMessage).
        B64.decode(sig).is_ok_and(|sig| hmac::verify(&key, auth_message.as_bytes(), &sig).is_ok())
    }
}

/// `=` and `,` are reserved in SCRAM attribute values (RFC 5802 §5.1).
fn escape(name: &str) -> String {
    name.replace('=', "=3D").replace(',', "=2C")
}

/// The value of single-letter attribute `key` in a `k=v,k=v` message (values
/// may themselves contain `=`).
fn attribute<'a>(message: &'a str, key: &str) -> Option<&'a str> {
    message
        .split(',')
        .find_map(|part| part.strip_prefix(key).and_then(|rest| rest.strip_prefix('=')))
        .filter(|_| key.len() == 1)
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> Vec<u8> {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), msg).as_ref().to_vec()
}

fn random_nonce() -> String {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 24];
    // SystemRandom only fails if the OS RNG is unavailable; a zero nonce is
    // still unique per server-side extension, but should never happen.
    let _ = ring::rand::SystemRandom::new().fill(&mut bytes);
    B64.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 7677 §3 test vector.
    const NONCE: &str = "rOprNGfwEbeRWgbNEkqO";
    const SERVER_FIRST: &str =
        "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096";

    #[test]
    fn rfc7677_vector() {
        let mut c = ScramClient::with_nonce("user", "pencil", NONCE.into());
        assert_eq!(c.client_first(), "n,,n=user,r=rOprNGfwEbeRWgbNEkqO");
        assert_eq!(
            c.client_final(SERVER_FIRST).unwrap(),
            "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ="
        );
        assert!(c.verify("v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4="));
    }

    #[test]
    fn forged_server_signature_is_rejected() {
        let mut c = ScramClient::with_nonce("user", "pencil", NONCE.into());
        c.client_first();
        c.client_final(SERVER_FIRST).unwrap();
        assert!(!c.verify("v=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="));
        assert!(!c.verify("e=invalid-proof"));
    }

    #[test]
    fn server_nonce_must_extend_ours() {
        let mut c = ScramClient::with_nonce("user", "pencil", NONCE.into());
        c.client_first();
        assert!(c.client_final("r=somethingelse,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096").is_none());
        let mut c = ScramClient::with_nonce("user", "pencil", NONCE.into());
        c.client_first();
        assert!(c.client_final(&format!("r={NONCE},s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096")).is_none());
    }

    #[test]
    fn username_is_escaped() {
        let mut c = ScramClient::with_nonce("a=b,c", "x", "n".into());
        assert_eq!(c.client_first(), "n,,n=a=3Db=2Cc,r=n");
    }
}
