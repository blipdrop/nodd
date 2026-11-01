//! ログイン。送信キーはこの実行だけの鍵で封緘されて届く。

use base64::{engine::general_purpose::STANDARD, Engine};
use hpke::{aead::ChaCha20Poly1305, kdf::HkdfSha256, kem::X25519HkdfSha256, Deserializable, Kem, OpModeR, Serializable};
use qrcode::{render::unicode, QrCode};
use rand::{rngs::StdRng, SeedableRng};

const INFO: &[u8] = b"nod-message-v1";

pub struct PairingKey {
    private: <X25519HkdfSha256 as Kem>::PrivateKey,
    public: <X25519HkdfSha256 as Kem>::PublicKey,
}

impl PairingKey {
    pub fn generate() -> Self {
        let (private, public) = X25519HkdfSha256::gen_keypair(&mut StdRng::from_entropy());
        Self { private, public }
    }

    pub fn public_base64(&self) -> String {
        STANDARD.encode(self.public.to_bytes())
    }

    pub fn open(&self, sealed: &str, code: &str) -> Result<String, String> {
        let bytes = STANDARD.decode(sealed).map_err(|e| e.to_string())?;
        if bytes.len() <= 32 {
            return Err("受け取ったキーの形式が正しくありません".to_string());
        }
        let (enc, ciphertext) = bytes.split_at(32);
        let enc = <X25519HkdfSha256 as Kem>::EncappedKey::from_bytes(enc).map_err(|e| e.to_string())?;
        let plain = hpke::single_shot_open::<ChaCha20Poly1305, HkdfSha256, X25519HkdfSha256>(
            &OpModeR::Base,
            &self.private,
            &enc,
            INFO,
            ciphertext,
            code.as_bytes(),
        )
        .map_err(|e| e.to_string())?;
        String::from_utf8(plain).map_err(|e| e.to_string())
    }
}

/// iPhoneが読むURL。接続先は載せず、アプリの環境へ必ず問い合わせる。
pub fn link(code: &str, name: &str) -> String {
    let mut url = url::Url::parse("nodd://pair").expect("static url");
    url.query_pairs_mut().append_pair("code", code).append_pair("name", name);
    url.to_string()
}

pub fn qr(text: &str) -> Result<String, String> {
    let code = QrCode::new(text).map_err(|e| e.to_string())?;
    Ok(code.render::<unicode::Dense1x2>().quiet_zone(true).build())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hpke::OpModeS;

    /// サーバーの封緘と同じ手順。
    fn seal(key: &PairingKey, code: &str, plaintext: &[u8]) -> String {
        let (enc, ciphertext) = hpke::single_shot_seal::<ChaCha20Poly1305, HkdfSha256, X25519HkdfSha256, _>(
            &OpModeS::Base,
            &key.public,
            INFO,
            plaintext,
            code.as_bytes(),
            &mut StdRng::from_entropy(),
        )
        .unwrap();
        let mut sealed = enc.to_bytes().to_vec();
        sealed.extend_from_slice(&ciphertext);
        STANDARD.encode(sealed)
    }

    #[test]
    fn opens_what_the_server_seals() {
        let key = PairingKey::generate();
        let sealed = seal(&key, "code1", b"nodd_test_s_abc");
        assert_eq!(key.open(&sealed, "code1").unwrap(), "nodd_test_s_abc");
        assert!(key.open(&sealed, "code2").is_err());
        assert!(PairingKey::generate().open(&sealed, "code1").is_err());
    }

    #[test]
    fn link_escapes_the_name() {
        let text = link("abc", "my mac&co");
        let parsed = url::Url::parse(&text).unwrap();
        let pairs: Vec<_> = parsed.query_pairs().into_owned().collect();
        assert_eq!(pairs, vec![("code".into(), "abc".into()), ("name".into(), "my mac&co".into())]);
        assert!(qr(&text).unwrap().lines().count() > 10);
    }
}
