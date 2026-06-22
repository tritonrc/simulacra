//! Native module definition for `simulacra:crypto`.
//!
//! Provides randomness and hashing backed by Rust crates.
//! Functions: `randomUUID`, `randomBytes`, `createHash`, `getRandomValues`.

use base64::Engine as _;
use rquickjs::module::{Declarations, Exports, ModuleDef};
use rquickjs::{Ctx, Function, Object};

pub struct CryptoModule;

impl ModuleDef for CryptoModule {
    fn declare<'js>(decl: &Declarations<'js>) -> rquickjs::Result<()> {
        decl.declare("randomUUID")?;
        decl.declare("randomBytes")?;
        decl.declare("createHash")?;
        decl.declare("getRandomValues")?;
        decl.declare("default")?;
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &Exports<'js>) -> rquickjs::Result<()> {
        // Register the Rust-backed digest helper on globals so createHash can use it.
        register_digest_helper(ctx)?;
        // Register the fill-random helper for getRandomValues.
        register_fill_random_helper(ctx)?;

        // randomUUID() -> string (UUID v4)
        let random_uuid_fn = Function::new(ctx.clone(), || -> String {
            uuid::Uuid::new_v4().to_string()
        })?;
        exports.export("randomUUID", random_uuid_fn.clone())?;

        // randomBytes(n) -> Uint8Array (Vec<u8> auto-converts)
        // Capped at 1 MiB to prevent unbounded memory allocation.
        const MAX_RANDOM_BYTES: usize = 1_048_576;
        let random_bytes_fn =
            Function::new(ctx.clone(), |n: usize| -> rquickjs::Result<Vec<u8>> {
                if n > MAX_RANDOM_BYTES {
                    return Err(rquickjs::Error::new_from_js_message(
                        "number",
                        "number",
                        &format!(
                            "randomBytes: requested {n} bytes exceeds maximum of {MAX_RANDOM_BYTES}"
                        ),
                    ));
                }
                use rand::RngCore;
                let mut buf = vec![0u8; n];
                rand::thread_rng().fill_bytes(&mut buf);
                Ok(buf)
            })?;
        exports.export("randomBytes", random_bytes_fn.clone())?;

        // createHash(algo) -> Hash object with .update(data) and .digest(encoding)
        // Implemented as a pure JS function that delegates to the Rust __simulacra_crypto_digest.
        // This avoids lifetime issues with returning JS objects from Rust closures.
        ctx.eval::<(), _>(
            r#"globalThis.__simulacra_createHash = function(algo) {
                if (algo !== 'sha256' && algo !== 'sha512' && algo !== 'md5') {
                    throw new Error("Error: unsupported hash algorithm: '" + algo + "'");
                }
                const obj = { __data: [], __algo: algo };
                obj.update = function(data) {
                    this.__data.push(String(data));
                    return this;
                };
                obj.digest = function(encoding) {
                    const combined = this.__data.join('');
                    if (encoding === undefined || encoding === null) {
                        // No encoding: return raw bytes as Uint8Array
                        return __simulacra_crypto_digest_raw(this.__algo, combined);
                    }
                    return __simulacra_crypto_digest(this.__algo, combined, encoding);
                };
                return obj;
            };"#,
        )?;
        let create_hash_fn: Function<'js> = ctx.globals().get("__simulacra_createHash")?;
        exports.export("createHash", create_hash_fn.clone())?;

        // getRandomValues(typedArray) -> fills typed array with random bytes, returns it
        // Implemented as a JS wrapper over __simulacra_fill_random
        ctx.eval::<(), _>(
            r#"globalThis.__simulacra_getRandomValues = function(typedArray) {
                const len = typedArray.byteLength;
                const bytes = __simulacra_fill_random(len);
                const view = new Uint8Array(typedArray.buffer, typedArray.byteOffset, typedArray.byteLength);
                for (let i = 0; i < bytes.length; i++) view[i] = bytes[i];
                return typedArray;
            };"#,
        )?;
        let get_random_values_fn: Function<'js> =
            ctx.globals().get("__simulacra_getRandomValues")?;
        exports.export("getRandomValues", get_random_values_fn.clone())?;

        // Default export: object with all functions
        let default_obj = Object::new(ctx.clone())?;
        default_obj.set("randomUUID", random_uuid_fn)?;
        default_obj.set("randomBytes", random_bytes_fn)?;
        default_obj.set("createHash", create_hash_fn)?;
        default_obj.set("getRandomValues", get_random_values_fn)?;
        exports.export("default", default_obj)?;

        Ok(())
    }
}

/// Register `__simulacra_crypto_digest(algo, data, encoding)` as a global Rust function.
fn register_digest_helper(ctx: &Ctx<'_>) -> rquickjs::Result<()> {
    use md5::Md5;
    use sha2::{Digest, Sha256, Sha512};

    let digest_fn = Function::new(
        ctx.clone(),
        |algo: String, data: String, encoding: String| -> rquickjs::Result<String> {
            let hash_bytes = match algo.as_str() {
                "sha256" => {
                    let mut hasher = Sha256::new();
                    hasher.update(data.as_bytes());
                    hasher.finalize().to_vec()
                }
                "sha512" => {
                    let mut hasher = Sha512::new();
                    hasher.update(data.as_bytes());
                    hasher.finalize().to_vec()
                }
                "md5" => {
                    let mut hasher = Md5::new();
                    hasher.update(data.as_bytes());
                    hasher.finalize().to_vec()
                }
                _ => {
                    return Err(rquickjs::Error::new_from_js_message(
                        "string",
                        "string",
                        &format!("unsupported algorithm: {algo}"),
                    ));
                }
            };

            match encoding.as_str() {
                "hex" => Ok(hex::encode(&hash_bytes)),
                "base64" => Ok(base64::engine::general_purpose::STANDARD.encode(&hash_bytes)),
                _ => Err(rquickjs::Error::new_from_js_message(
                    "string",
                    "string",
                    &format!("unsupported encoding: '{encoding}'. Use 'hex' or 'base64'."),
                )),
            }
        },
    )?;
    ctx.globals().set("__simulacra_crypto_digest", digest_fn)?;

    // Raw-bytes variant: returns Vec<u8> which rquickjs converts to Uint8Array.
    let digest_raw_fn = Function::new(
        ctx.clone(),
        |algo: String, data: String| -> rquickjs::Result<Vec<u8>> {
            let hash_bytes = match algo.as_str() {
                "sha256" => {
                    let mut hasher = Sha256::new();
                    hasher.update(data.as_bytes());
                    hasher.finalize().to_vec()
                }
                "sha512" => {
                    let mut hasher = Sha512::new();
                    hasher.update(data.as_bytes());
                    hasher.finalize().to_vec()
                }
                "md5" => {
                    let mut hasher = Md5::new();
                    hasher.update(data.as_bytes());
                    hasher.finalize().to_vec()
                }
                _ => {
                    return Err(rquickjs::Error::new_from_js_message(
                        "string",
                        "string",
                        &format!("unsupported algorithm: {algo}"),
                    ));
                }
            };
            Ok(hash_bytes)
        },
    )?;
    ctx.globals()
        .set("__simulacra_crypto_digest_raw", digest_raw_fn)?;
    Ok(())
}

/// Register `__simulacra_fill_random(n)` as a global Rust function for getRandomValues.
fn register_fill_random_helper(ctx: &Ctx<'_>) -> rquickjs::Result<()> {
    let fill_fn = Function::new(ctx.clone(), |n: usize| -> rquickjs::Result<Vec<u8>> {
        if n > 65536 {
            return Err(rquickjs::Error::new_from_js_message(
                "number",
                "number",
                "QuotaExceededError: getRandomValues: array exceeds 65536 bytes",
            ));
        }
        use rand::RngCore;
        let mut buf = vec![0u8; n];
        rand::thread_rng().fill_bytes(&mut buf);
        Ok(buf)
    })?;
    ctx.globals().set("__simulacra_fill_random", fill_fn)?;
    Ok(())
}
