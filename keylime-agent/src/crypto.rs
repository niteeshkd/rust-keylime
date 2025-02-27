// SPDX-License-Identifier: Apache-2.0
// Copyright 2021 Keylime Authors

use base64::{engine::general_purpose, Engine as _};
use log::*;
use openssl::{
    asn1::Asn1Time,
    encrypt::Decrypter,
    hash::MessageDigest,
    memcmp,
    nid::Nid,
    pkcs5,
    pkey::{Id, PKey, PKeyRef, Private, Public},
    rsa::{Padding, Rsa},
    sign::{Signer, Verifier},
    ssl::{SslAcceptor, SslAcceptorBuilder, SslMethod, SslVerifyMode},
    symm::Cipher,
    x509::store::X509StoreBuilder,
    x509::{X509Name, X509},
};
use picky_asn1_x509::SubjectPublicKeyInfo;
use std::{
    fs::{read_to_string, set_permissions, File, Permissions},
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    path::Path,
    string::String,
};

use crate::{
    Error, Result, AES_128_KEY_LEN, AES_256_KEY_LEN, AES_BLOCK_SIZE,
};

// Read a X509 cert in DER format from path
pub(crate) fn load_x509_der(input_cert_path: &Path) -> Result<X509> {
    let contents = std::fs::read(input_cert_path).map_err(Error::from)?;

    X509::from_der(&contents).map_err(Error::Crypto)
}

pub(crate) fn load_x509_pem(input_cert_path: &Path) -> Result<X509> {
    let contents = std::fs::read(input_cert_path).map_err(Error::from)?;

    X509::from_pem(&contents).map_err(Error::Crypto)
}

// Read a X509 cert or cert chain and outputs the first certificate
pub(crate) fn load_x509(input_cert_path: &Path) -> Result<X509> {
    let mut cert_chain = load_x509_cert_chain(input_cert_path)?;

    if cert_chain.len() != 1 {
        return Err(Error::Other(
            "More than one public key provided in revocation cert"
                .to_string(),
        ));
    }
    let cert = cert_chain.pop().unwrap(); //#[allow_ci]

    Ok(cert)
}

fn load_x509_cert_chain(input_cert_path: &Path) -> Result<Vec<X509>> {
    let contents = read_to_string(input_cert_path).map_err(Error::from)?;

    X509::stack_from_pem(contents.as_bytes()).map_err(Error::Crypto)
}

pub(crate) fn load_x509_cert_list(
    input_cert_list: Vec<&Path>,
) -> Result<Vec<X509>> {
    let mut loaded = Vec::<X509>::new();
    for cert in input_cert_list {
        match load_x509_cert_chain(cert) {
            Ok(mut s) => {
                loaded.append(&mut s);
            }
            Err(e) => {
                warn!("Could not load certs from {}: {}", cert.display(), e);
            }
        }
    }
    Ok(loaded)
}

/// Write a X509 certificate to a file in PEM format
pub(crate) fn write_x509(cert: &X509, file_path: &Path) -> Result<()> {
    let mut file = std::fs::File::create(file_path)?;
    _ = file.write(&cert.to_pem()?)?;
    Ok(())
}

/// Check an x509 certificate contains a specific public key
pub(crate) fn check_x509_key(
    cert: &X509,
    tpm_key: tss_esapi::structures::Public,
) -> Result<bool> {
    // Id:RSA_PSS only added in rust-openssl from v0.10.59; remove this let and use Id::RSA_PSS after update
    // Id taken from https://boringssl.googlesource.com/boringssl/+/refs/heads/master/include/openssl/nid.h#4039
    let id_rsa_pss: Id = Id::from_raw(912);
    match cert.public_key()?.id() {
        Id::RSA => {
            let cert_n = cert.public_key()?.rsa()?.n().to_vec();
            let mut cert_n_str = format!("{:?}", cert_n);
            _ = cert_n_str.pop();
            _ = cert_n_str.remove(0);
            let key = SubjectPublicKeyInfo::try_from(tpm_key)?;
            let key_der = picky_asn1_der::to_vec(&key)?;
            let key_der_str = format!("{:?}", key_der);

            Ok(key_der_str.contains(&cert_n_str))
        }
        cert_id if cert_id == id_rsa_pss => {
            let cert_n = cert.public_key()?.rsa()?.n().to_vec();
            let mut cert_n_str = format!("{:?}", cert_n);
            _ = cert_n_str.pop();
            _ = cert_n_str.remove(0);
            let key = SubjectPublicKeyInfo::try_from(tpm_key)?;
            let key_der = picky_asn1_der::to_vec(&key)?;
            let key_der_str = format!("{:?}", key_der);

            Ok(key_der_str.contains(&cert_n_str))
        }
        Id::EC => {
            let cert_n = cert.public_key()?.ec_key()?.public_key_to_der()?;
            let mut cert_n_str = format!("{:?}", cert_n);
            _ = cert_n_str.pop();
            _ = cert_n_str.remove(0);
            let key = SubjectPublicKeyInfo::try_from(tpm_key)?;
            let key_der = picky_asn1_der::to_vec(&key)?;
            let key_der_str = format!("{:?}", key_der);

            Ok(key_der_str.contains(&cert_n_str))
        }
        _ => Err(Error::Other(
            "Certificate does not seem to have an RSA or EC key".to_string(),
        )),
    }
}

/// Detect a template from a certificate
/// Templates defined in: TPM 2.0 Keys for Device Identity and Attestation at https://trustedcomputinggroup.org/wp-content/uploads/TPM-2p0-Keys-for-Device-Identity-and-Attestation_v1_r12_pub10082021.pdf
pub(crate) fn match_cert_to_template(cert: &X509) -> Result<String> {
    // Id:RSA_PSS only added in rust-openssl from v0.10.59; remove this let and use Id::RSA_PSS after update
    // Id taken from https://boringssl.googlesource.com/boringssl/+/refs/heads/master/include/openssl/nid.h#4039
    let id_rsa_pss: Id = Id::from_raw(912);
    match cert.public_key()?.id() {
        Id::RSA => match cert.public_key()?.bits() {
            2048 => Ok("H-1".to_string()),
            _ => Ok("".to_string()),
        },
        cert_id if cert_id == id_rsa_pss => match cert.public_key()?.bits() {
            2048 => Ok("H-1".to_string()),
            _ => Ok("".to_string()),
        },
        Id::EC => match cert.public_key()?.bits() {
            256 => match cert.public_key()?.ec_key()?.group().curve_name() {
                Some(Nid::SECP256K1) => Ok("H-2".to_string()),
                _ => Ok("H-5".to_string()),
            },
            384 => Ok("H-3".to_string()),
            521 => Ok("H-4".to_string()),
            _ => Ok("".to_string()),
        },
        _ => Err(Error::Other(
            "Certificate does not seem to have an RSA or EC key".to_string(),
        )),
    }
}

/// Read a PEM file and returns the public and private keys
pub(crate) fn load_key_pair(
    key_path: &Path,
    key_password: Option<&str>,
) -> Result<(PKey<Public>, PKey<Private>)> {
    let pem = std::fs::read(key_path)?;
    let private = match key_password {
        Some(pw) => {
            if pw.is_empty() {
                PKey::private_key_from_pem(&pem)?
            } else {
                PKey::private_key_from_pem_passphrase(&pem, pw.as_bytes())?
            }
        }
        None => PKey::private_key_from_pem(&pem)?,
    };
    let public = pkey_pub_from_priv(private.clone())?;
    Ok((public, private))
}

/// Write a private key to a file.
///
/// If a passphrase is provided, the key will be stored encrypted using AES-256-CBC
pub(crate) fn write_key_pair(
    key: &PKey<Private>,
    file_path: &Path,
    passphrase: Option<&str>,
) -> Result<()> {
    // Write the generated key to the file
    let mut file = std::fs::File::create(file_path)?;
    match passphrase {
        Some(pw) => {
            if pw.is_empty() {
                _ = file.write(&key.private_key_to_pem_pkcs8()?)?;
            } else {
                _ = file.write(&key.private_key_to_pem_pkcs8_passphrase(
                    openssl::symm::Cipher::aes_256_cbc(),
                    pw.as_bytes(),
                )?)?;
            }
        }
        None => {
            _ = file.write(&key.private_key_to_pem_pkcs8()?)?;
        }
    }
    set_permissions(file_path, Permissions::from_mode(0o600))?;
    Ok(())
}

fn rsa_generate(key_size: u32) -> Result<PKey<Private>> {
    PKey::from_rsa(Rsa::generate(key_size)?).map_err(Error::Crypto)
}

pub(crate) fn rsa_generate_pair(
    key_size: u32,
) -> Result<(PKey<Public>, PKey<Private>)> {
    let private = rsa_generate(key_size)?;
    let public = pkey_pub_from_priv(private.clone())?;
    Ok((public, private))
}

fn pkey_pub_from_priv(privkey: PKey<Private>) -> Result<PKey<Public>> {
    match privkey.id() {
        Id::RSA => {
            let rsa = Rsa::from_public_components(
                privkey.rsa()?.n().to_owned()?,
                privkey.rsa()?.e().to_owned()?,
            )
            .map_err(Error::Crypto)?;
            PKey::from_rsa(rsa).map_err(Error::Crypto)
        }
        id => Err(Error::Other(format!(
            "pkey_pub_from_priv not yet implemented for key type {id:?}"
        ))),
    }
}

pub(crate) fn pkey_pub_to_pem(pubkey: &PKey<Public>) -> Result<String> {
    pubkey
        .public_key_to_pem()
        .map_err(Error::from)
        .and_then(|s| String::from_utf8(s).map_err(Error::from))
}

pub(crate) fn generate_x509(key: &PKey<Private>, uuid: &str) -> Result<X509> {
    let mut name = X509Name::builder()?;
    name.append_entry_by_nid(Nid::COMMONNAME, uuid)?;
    let name = name.build();

    let valid_from = Asn1Time::days_from_now(0)?;
    let valid_to = Asn1Time::days_from_now(356)?;

    let mut builder = X509::builder()?;
    builder.set_version(2)?;
    builder.set_subject_name(&name)?;
    builder.set_issuer_name(&name)?;
    builder.set_not_before(&valid_from)?;
    builder.set_not_after(&valid_to)?;
    builder.set_pubkey(key)?;
    builder.sign(key, MessageDigest::sha256())?;

    Ok(builder.build())
}

pub(crate) fn generate_mtls_context(
    mtls_cert: &X509,
    key: &PKey<Private>,
    keylime_ca_certs: Vec<X509>,
) -> Result<SslAcceptorBuilder> {
    let mut ssl_context_builder =
        SslAcceptor::mozilla_intermediate(SslMethod::tls())?;
    ssl_context_builder.set_certificate(mtls_cert);
    ssl_context_builder.set_private_key(key);

    // Build verification cert store.
    let mut mtls_store_builder = X509StoreBuilder::new()?;
    for cert in keylime_ca_certs {
        mtls_store_builder.add_cert(cert)?;
    }

    let mtls_store = mtls_store_builder.build();
    ssl_context_builder.set_verify_cert_store(mtls_store);

    // Enable mTLS verification
    let mut verify_mode = SslVerifyMode::empty();
    verify_mode.set(SslVerifyMode::PEER, true);
    verify_mode.set(SslVerifyMode::FAIL_IF_NO_PEER_CERT, true);
    ssl_context_builder.set_verify(verify_mode);

    Ok(ssl_context_builder)
}

/*
 * Inputs: password to derive key
 *         shared salt
 * Output: derived key
 *
 * Take in a password and shared salt, and derive a key based on the
 * PBKDF2-HMAC key derivation function. Parameters match that of
 * Python-Keylime.
 *
 * NOTE: This uses SHA-1 as the KDF's hash function in order to match the
 * implementation of PBKDF2 in the Python version of Keylime. PyCryptodome's
 * PBKDF2 function defaults to SHA-1 unless otherwise specified, and
 * Python-Keylime uses this default.
 */
pub(crate) fn kdf(
    input_password: String,
    input_salt: String,
) -> Result<String> {
    let password = input_password.as_bytes();
    let salt = input_salt.as_bytes();
    let count = 2000;
    // PyCryptodome's PBKDF2 binding allows key length to be specified
    // explicitly as a parameter; here, key length is implicitly defined in
    // the length of the 'key' variable.
    let mut key = [0; 32];
    pkcs5::pbkdf2_hmac(
        password,
        salt,
        count,
        MessageDigest::sha1(),
        &mut key,
    )?;
    Ok(hex::encode(&key[..]))
}

/*
 * Input: Trusted public key, and remote message and signature
 * Output: true if they are verified, otherwise false
 *
 * Verify a remote message and signature against a local rsa cert
 */
pub(crate) fn asym_verify(
    keypair: &PKeyRef<Public>,
    message: &str,
    signature: &str,
) -> Result<bool> {
    let mut verifier = Verifier::new(MessageDigest::sha256(), keypair)?;
    verifier.set_rsa_padding(Padding::PKCS1_PSS)?;
    verifier.set_rsa_mgf1_md(MessageDigest::sha256())?;
    verifier
        .set_rsa_pss_saltlen(openssl::sign::RsaPssSaltlen::MAXIMUM_LENGTH)?;
    verifier.update(message.as_bytes())?;
    Ok(verifier
        .verify(&general_purpose::STANDARD.decode(signature.as_bytes())?)?)
}

/*
 * Inputs: OpenSSL RSA key
 *         ciphertext to be decrypted
 * Output: decrypted plaintext
 *
 * Take in an RSA-encrypted ciphertext and an RSA private key and decrypt the
 * ciphertext based on PKCS1 OAEP.
 */
pub(crate) fn rsa_oaep_decrypt(
    priv_key: &PKey<Private>,
    data: &[u8],
) -> Result<Vec<u8>> {
    let mut decrypter = Decrypter::new(priv_key)?;

    decrypter.set_rsa_padding(Padding::PKCS1_OAEP)?;
    decrypter.set_rsa_mgf1_md(MessageDigest::sha1())?;
    decrypter.set_rsa_oaep_md(MessageDigest::sha1())?;

    // Create an output buffer
    let buffer_len = decrypter.decrypt_len(data)?;
    let mut decrypted = vec![0; buffer_len];

    // Decrypt and truncate the buffer
    let decrypted_len = decrypter.decrypt(data, &mut decrypted)?;
    decrypted.truncate(decrypted_len);

    Ok(decrypted)
}

/*
 * Inputs: secret key
 *        message to sign
 * Output: signed HMAC result
 *
 * Sign message and return HMAC result string
 */
pub(crate) fn compute_hmac(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let pkey = PKey::hmac(key)?;
    // SHA-384 is used as the underlying hash algorithm.
    //
    // Reference:
    // https://keylime-docs.readthedocs.io/en/latest/rest_apis.html#post--v1.0-keys-ukey
    // https://github.com/keylime/keylime/blob/910b38b296038b187a020c095dc747e9c46cbef3/keylime/crypto.py#L151
    let mut signer = Signer::new(MessageDigest::sha384(), &pkey)?;
    signer.update(data)?;
    signer.sign_to_vec().map_err(Error::Crypto)
}

pub(crate) fn verify_hmac(
    key: &[u8],
    data: &[u8],
    hmac: &[u8],
) -> Result<()> {
    let pkey = PKey::hmac(key)?;
    // SHA-384 is used as the underlying hash algorithm.
    //
    // Reference:
    // https://keylime-docs.readthedocs.io/en/latest/rest_apis.html#post--v1.0-keys-ukey
    // https://github.com/keylime/keylime/blob/910b38b296038b187a020c095dc747e9c46cbef3/keylime/crypto.py#L151
    let mut signer = Signer::new(MessageDigest::sha384(), &pkey)?;
    signer.update(data)?;

    if !memcmp::eq(&signer.sign_to_vec()?, hmac) {
        return Err(Error::Other("hmac check failed".to_string()));
    }

    Ok(())
}

pub(crate) fn decrypt_aead(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let cipher = match key.len() {
        AES_128_KEY_LEN => Cipher::aes_128_gcm(),
        AES_256_KEY_LEN => Cipher::aes_256_gcm(),
        other => {
            return Err(Error::Other(format!(
                "key length {other} does not correspond to valid GCM cipher"
            )))
        }
    };

    // Parse out payload IV, tag, ciphertext.  Note that Keylime
    // currently uses 16-byte IV, while the recommendation in SP
    // 800-38D is 12-byte.
    //
    // Reference:
    // https://github.com/keylime/keylime/blob/1663a7702b3286152b38dbcb715a9eb6705e05e9/keylime/crypto.py#L191
    if data.len() < AES_BLOCK_SIZE * 2 {
        return Err(Error::InvalidRequest);
    }
    let (iv, rest) = data.split_at(AES_BLOCK_SIZE);
    let (ciphertext, tag) = rest.split_at(rest.len() - AES_BLOCK_SIZE);

    openssl::symm::decrypt_aead(cipher, key, Some(iv), &[], ciphertext, tag)
        .map_err(Error::Crypto)
}

pub mod testing {
    use super::*;
    use openssl::encrypt::Encrypter;
    use std::path::Path;

    pub(crate) fn rsa_import_pair(
        path: impl AsRef<Path>,
    ) -> Result<(PKey<Public>, PKey<Private>)> {
        let contents = read_to_string(path)?;
        let private = PKey::private_key_from_pem(contents.as_bytes())?;
        let public = pkey_pub_from_priv(private.clone())?;
        Ok((public, private))
    }

    pub(crate) fn pkey_pub_from_pem(pem: &str) -> Result<PKey<Public>> {
        PKey::<Public>::public_key_from_pem(pem.as_bytes())
            .map_err(Error::Crypto)
    }

    pub(crate) fn rsa_oaep_encrypt(
        pub_key: &PKey<Public>,
        data: &[u8],
    ) -> Result<Vec<u8>> {
        let mut encrypter = Encrypter::new(pub_key)?;

        encrypter.set_rsa_padding(Padding::PKCS1_OAEP)?;
        encrypter.set_rsa_mgf1_md(MessageDigest::sha1())?;
        encrypter.set_rsa_oaep_md(MessageDigest::sha1())?;

        // Create an output buffer
        let buffer_len = encrypter.encrypt_len(data)?;
        let mut encrypted = vec![0; buffer_len];

        // Encrypt and truncate the buffer
        let encrypted_len = encrypter.encrypt(data, &mut encrypted)?;
        encrypted.truncate(encrypted_len);

        Ok(encrypted)
    }

    pub(crate) fn encrypt_aead(
        key: &[u8],
        iv: &[u8],
        data: &[u8],
    ) -> Result<Vec<u8>> {
        let cipher = match key.len() {
            AES_128_KEY_LEN => Cipher::aes_128_gcm(),
            AES_256_KEY_LEN => Cipher::aes_256_gcm(),
            other => {
                return Err(Error::Other(format!(
                "key length {other} does not correspond to valid GCM cipher"
            )))
            }
        };
        if iv.len() != AES_BLOCK_SIZE {
            return Err(Error::Other(format!(
                "IV length {} does not correspond to valid GCM cipher {}",
                iv.len(),
                AES_BLOCK_SIZE
            )));
        }
        let mut tag = vec![0u8; AES_BLOCK_SIZE];
        let ciphertext = openssl::symm::encrypt_aead(
            cipher,
            key,
            Some(iv),
            &[],
            data,
            &mut tag,
        )
        .map_err(Error::Crypto)?;
        let mut result =
            Vec::with_capacity(iv.len() + ciphertext.len() + tag.len());
        result.extend(iv);
        result.extend(ciphertext);
        result.extend(tag);
        Ok(result)
    }

    pub(crate) fn rsa_generate(key_size: u32) -> Result<PKey<Private>> {
        super::rsa_generate(key_size)
    }
}

// Unit Testing
#[cfg(test)]
mod tests {
    use super::*;
    use openssl::rsa::Rsa;
    use std::{fs, path::Path};
    use testing::{encrypt_aead, rsa_import_pair, rsa_oaep_encrypt};

    // compare with the result from python output
    #[test]
    fn test_compute_hmac() {
        let key = String::from("mysecret");
        let message = String::from("hellothere");
        let mac =
            compute_hmac(key.as_bytes(), message.as_bytes()).map(hex::encode);
        assert_eq!(
            format!(
                "{}{}",
                "b8558314f515931c8d9b329805978fe77b9bb020b05406c0e",
                "f189d89846ff8f5f0ca10e387d2c424358171df7f896f9f"
            ),
            mac.unwrap() //#[allow_ci]
        );
    }

    // Test KDF to ensure derived password matches result derived from Python
    // functions.
    #[test]
    fn test_kdf() {
        let password = String::from("myverysecretsecret");
        let salt = String::from("thesaltiestsalt");
        let key = kdf(password, salt);
        assert_eq!(
            "8a6de415abb8b27de5c572c8137bd14e5658395f9a2346e0b1ad8b9d8b9028af"
                .to_string(),
            key.unwrap() //#[allow_ci]
        );
    }

    #[test]
    fn test_hmac_verification() {
        // Generate a keypair
        let (pub_key, priv_key) = rsa_generate_pair(2048).unwrap(); //#[allow_ci]
        let data = b"hello, world!";
        let data2 = b"hola, mundo!";

        // Sign the data
        let mut signer =
            Signer::new(MessageDigest::sha256(), &priv_key).unwrap(); //#[allow_ci]
        signer.update(data).unwrap(); //#[allow_ci]
        signer.update(data2).unwrap(); //#[allow_ci]
        let signature = signer.sign_to_vec().unwrap(); //#[allow_ci]

        // Verify the data
        let mut verifier =
            Verifier::new(MessageDigest::sha256(), &pub_key).unwrap(); //#[allow_ci]
        verifier.update(data).unwrap(); //#[allow_ci]
        verifier.update(data2).unwrap(); //#[allow_ci]
        assert!(verifier.verify(&signature).unwrap()); //#[allow_ci]
    }

    #[test]
    fn test_rsa_oaep() {
        // Import a keypair
        let rsa_key_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test-data")
            .join("test-rsa.pem");

        let (pub_key, priv_key) = rsa_import_pair(rsa_key_path)
            .expect("unable to import RSA key pair");
        let plaintext = b"0123456789012345";
        let ciphertext = rsa_oaep_encrypt(&pub_key, &plaintext[..])
            .expect("unable to encrypt");

        // We can't check against the fixed ciphertext, as OAEP
        // involves randomness. Check with a round-trip instead.
        let decrypted = rsa_oaep_decrypt(&priv_key, &ciphertext[..])
            .expect("unable to decrypt");
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_encrypt_aead_short() {
        let key = b"0123456789012345";
        let iv = b"ABCDEFGHIJKLMNOP";
        let plaintext = b"test string, longer than the block size";
        let ciphertext = encrypt_aead(&key[..], &iv[..], &plaintext[..])
            .expect("unable to encrypt");
        let expected = hex::decode("4142434445464748494A4B4C4D4E4F50B2198661586C9839CCDD0B1D5B4FF92FA9C0E6477C4E8E42C19ACD9E8061DD1E759401337DA285A70580E6A2E10B5D3A09994F46D90AB6").unwrap(); //#[allow_ci]
        assert_eq!(ciphertext, expected);
    }

    #[test]
    fn test_decrypt_aead_short() {
        let key = b"0123456789012345";
        let ciphertext = hex::decode("4142434445464748494A4B4C4D4E4F50B2198661586C9839CCDD0B1D5B4FF92FA9C0E6477C4E8E42C19ACD9E8061DD1E759401337DA285A70580E6A2E10B5D3A09994F46D90AB6").unwrap(); //#[allow_ci]
        let plaintext = decrypt_aead(&key[..], &ciphertext[..])
            .expect("unable to decrypt");
        let expected = b"test string, longer than the block size";
        assert_eq!(plaintext, expected);
    }

    #[test]
    fn test_encrypt_aead_long() {
        let key = b"01234567890123450123456789012345";
        let iv = b"ABCDEFGHIJKLMNOP";
        let plaintext = b"test string, longer than the block size";
        let ciphertext = encrypt_aead(&key[..], &iv[..], &plaintext[..])
            .expect("unable to encrypt");
        let expected = hex::decode("4142434445464748494A4B4C4D4E4F50FCE7CA78C08FB1D5E04DB3C4AA6B6ED2F09C4AD7985BD1DB9FF15F9FDA869D0C01B27FF4618737BB53C84D256455AAB53B9AC7EAF88C4B").unwrap(); //#[allow_ci]
        assert_eq!(ciphertext, expected);
    }

    #[test]
    fn test_decrypt_aead_long() {
        let key = b"01234567890123450123456789012345";
        let ciphertext = hex::decode("4142434445464748494A4B4C4D4E4F50FCE7CA78C08FB1D5E04DB3C4AA6B6ED2F09C4AD7985BD1DB9FF15F9FDA869D0C01B27FF4618737BB53C84D256455AAB53B9AC7EAF88C4B").unwrap(); //#[allow_ci]
        let plaintext = decrypt_aead(&key[..], &ciphertext[..])
            .expect("unable to decrypt");
        let expected = b"test string, longer than the block size";
        assert_eq!(plaintext, expected);
    }

    #[test]
    fn test_encrypt_aead_invalid_key_length() {
        let key = b"0123456789012345012345678901234";
        let iv = b"ABCDEFGHIJKLMNOP";
        let plaintext = b"test string, longer than the block size";
        let result = encrypt_aead(&key[..], &iv[..], &plaintext[..]);
        assert!(result.is_err())
    }

    #[test]
    fn test_encrypt_aead_invalid_iv_length() {
        let key = b"01234567890123450123456789012345";
        let iv = b"ABCDEFGHIJKLMN";
        let plaintext = b"test string, longer than the block size";
        let result = encrypt_aead(&key[..], &iv[..], &plaintext[..]);
        assert!(result.is_err())
    }

    #[test]
    fn test_decrypt_aead_invalid_key_length() {
        let key = b"0123456789012345012345678901234";
        let ciphertext = hex::decode("4142434445464748494A4B4C4D4E4F50FCE7CA78C08FB1D5E04DB3C4AA6B6ED2F09C4AD7985BD1DB9FF15F9FDA869D0C01B27FF4618737BB53C84D256455AAB53B9AC7EAF88C4B").unwrap(); //#[allow_ci]
        let result = decrypt_aead(&key[..], &ciphertext[..]);
        assert!(result.is_err())
    }

    #[test]
    fn test_decrypt_aead_invalid_ciphertext_length() {
        let key = b"0123456789012345";
        let ciphertext = hex::decode("41424344").unwrap(); //#[allow_ci]
        let result = decrypt_aead(&key[..], &ciphertext[..]);
        assert!(matches!(result, Err(Error::InvalidRequest)));
    }

    #[test]
    fn test_asym_verify() {
        // Import test keypair
        let rsa_key_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test-data")
            .join("test-rsa.pem");

        // Get RSA keys
        let contents = read_to_string(rsa_key_path);
        let private =
            PKey::private_key_from_pem(contents.unwrap().as_bytes()).unwrap(); //#[allow_ci]
        let public = pkey_pub_from_priv(private).unwrap(); //#[allow_ci]

        let message = String::from("Hello World!");

        // Get known valid signature
        let signature_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test-data")
            .join("test-rsa.sig");

        let signature = read_to_string(signature_path).unwrap(); //#[allow_ci]

        assert!(asym_verify(&public, &message, &signature).unwrap()) //#[allow_ci]
    }

    #[test]
    fn test_password() {
        // Import test keypair
        let rsa_key_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test-data")
            .join("test-rsa.pem");

        // Get RSA keys
        let (public, private) = rsa_import_pair(rsa_key_path).unwrap(); //#[allow_ci]

        // Create temporary directory and files names
        let temp_dir = tempfile::tempdir().unwrap(); //#[allow_ci]
        let encrypted_path =
            Path::new(&temp_dir.path()).join("encrypted.pem");
        let empty_pw_path = Path::new(&temp_dir.path()).join("empty_pw.pem");
        let none_pw_path = Path::new(&temp_dir.path()).join("none_pw.pem");

        let message = b"Hello World!";

        // Write keys to files
        assert!(write_key_pair(&private, &encrypted_path, Some("password"))
            .is_ok());
        assert!(write_key_pair(&private, &empty_pw_path, Some("")).is_ok());
        assert!(write_key_pair(&private, &none_pw_path, None).is_ok());

        // Read keys from files
        let (_, priv_from_encrypted) =
            load_key_pair(&encrypted_path, Some("password")).unwrap(); //#[allow_ci]
        let (_, priv_from_empty) =
            load_key_pair(&empty_pw_path, Some("")).unwrap(); //#[allow_ci]
        let (_, priv_from_none) = load_key_pair(&none_pw_path, None).unwrap(); //#[allow_ci]

        for keypair in [
            priv_from_encrypted.as_ref(),
            priv_from_empty.as_ref(),
            priv_from_none.as_ref(),
        ] {
            // Sign the data
            let mut signer =
                Signer::new(MessageDigest::sha256(), keypair).unwrap(); //#[allow_ci]
            signer.update(message).unwrap(); //#[allow_ci]
            let signature = signer.sign_to_vec().unwrap(); //#[allow_ci]

            // Verify the data
            let mut verifier =
                Verifier::new(MessageDigest::sha256(), keypair).unwrap(); //#[allow_ci]
            verifier.update(message).unwrap(); //#[allow_ci]
            assert!(verifier.verify(&signature).unwrap()); //#[allow_ci]
        }
    }

    #[test]
    fn test_x509() {
        let tempdir = tempfile::tempdir().unwrap(); //#[allow_ci]

        let (pubkey, privkey) = rsa_generate_pair(2048).unwrap(); //#[allow_ci]

        let r = generate_x509(&privkey, "uuidA");
        assert!(r.is_ok());
        let cert_a = r.unwrap(); //#[allow_ci]
        let cert_a_path = tempdir.path().join("cert_a.pem");
        let r = write_x509(&cert_a, &cert_a_path);
        assert!(r.is_ok());
        assert!(cert_a_path.exists());

        let r = generate_x509(&privkey, "uuidB");
        assert!(r.is_ok());
        let cert_b = r.unwrap(); //#[allow_ci]
        let cert_b_path = tempdir.path().join("cert_b.pem");
        let r = write_x509(&cert_b, &cert_b_path);
        assert!(r.is_ok());
        assert!(cert_b_path.exists());

        let loaded_a = load_x509(&cert_a_path);
        assert!(loaded_a.is_ok());
        let loaded_a = loaded_a.unwrap(); //#[allow_ci]

        let a_str = read_to_string(&cert_a_path).unwrap(); //#[allow_ci]
        let b_str = read_to_string(&cert_b_path).unwrap(); //#[allow_ci]
        let concat = a_str + &b_str;
        let concat_path = tempdir.path().join("concat.pem");
        fs::write(&concat_path, concat).unwrap(); //#[allow_ci]

        // Expect error as there are more than one certificate
        let r = load_x509(&concat_path);
        assert!(r.is_err());

        // Loading multiple certs should work when loading chain
        let r = load_x509_cert_chain(&concat_path);
        assert!(r.is_ok());
        let chain = r.unwrap(); //#[allow_ci]
        assert!(chain.len() == 2);

        // Test adding loading certs from a list, including an non-existing file
        let non_existing =
            Path::new("/non_existing_path/non_existing_cert.pem");
        let cert_list: Vec<&Path> =
            vec![&cert_a_path, non_existing, &cert_b_path];
        let r = load_x509_cert_list(cert_list);
        assert!(r.is_ok());
        let loaded_list = r.unwrap(); //#[allow_ci]
        assert!(loaded_list.len() == 2);

        let r = generate_mtls_context(&loaded_a, &privkey, loaded_list);
        assert!(r.is_ok());
    }
}
