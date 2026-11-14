//! Cache key descriptor (spec §7.4). Computed only here, with canonical JSON + SHA-256.

use crate::domain::canonical_json::canonical_sha256;
use crate::domain::versions;
use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct DescriptorInput<'a> {
    pub document_id: &'a str,
    pub document_sha256: &'a str,
    pub extraction_id: &'a str,
    pub block_id: &'a str,
    pub source_hash: &'a str,
    pub source_language: &'a str,
    pub target_language: &'a str,
    pub provider_kind: &'a str,
    pub endpoint_identity: &'a str,
    pub account_namespace: &'a str,
    pub model_id: &'a str,
    pub model_fingerprint: &'a str,
    pub profile_hash: &'a str,
    pub prompt_hash: &'a str,
    pub glossary_hash: &'a str,
    pub context_hash: &'a str,
    pub request_manifest_hash: &'a str,
}

pub fn descriptor(i: &DescriptorInput) -> Value {
    json!({
        "keyVersion": versions::CACHE_KEY,
        "documentId": i.document_id,
        "documentSha256": i.document_sha256,
        "extractionId": i.extraction_id,
        "blockId": i.block_id,
        "sourceHash": i.source_hash,
        "sourceLanguage": i.source_language,
        "targetLanguage": i.target_language,
        "providerKind": i.provider_kind,
        "endpointIdentity": i.endpoint_identity,
        "accountNamespace": i.account_namespace,
        "modelId": i.model_id,
        "modelFingerprint": i.model_fingerprint,
        "profileHash": i.profile_hash,
        "promptHash": i.prompt_hash,
        "glossaryHash": i.glossary_hash,
        "contextHash": i.context_hash,
        "splitterVersion": versions::SPLITTER,
        "placeholderVersion": versions::PLACEHOLDER,
        "adapterVersion": versions::ADAPTER,
        "requestManifestHash": i.request_manifest_hash,
    })
}

pub fn cache_key(desc: &Value) -> String {
    canonical_sha256(desc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> DescriptorInput<'static> {
        DescriptorInput {
            document_id: "doc-id",
            document_sha256: "sha256",
            extraction_id: "extraction-id",
            block_id: "block-id",
            source_hash: "sha256-of-sourceText",
            source_language: "en",
            target_language: "ko",
            provider_kind: "ollama",
            endpoint_identity: "http://127.0.0.1:11434",
            account_namespace: "local-default",
            model_id: "user-selected-model",
            model_fingerprint: "digest-or-user-cache-epoch",
            profile_hash: "p",
            prompt_hash: "q",
            glossary_hash: "g",
            context_hash: "c",
            request_manifest_hash: "m",
        }
    }

    /// Golden fixture: the descriptor serialization and key must never change silently.
    #[test]
    fn golden_key() {
        let d = descriptor(&base());
        let s = crate::domain::canonical_json::to_canonical_string(&d);
        assert_eq!(
            s,
            r#"{"accountNamespace":"local-default","adapterVersion":"1","blockId":"block-id","contextHash":"c","documentId":"doc-id","documentSha256":"sha256","endpointIdentity":"http://127.0.0.1:11434","extractionId":"extraction-id","glossaryHash":"g","keyVersion":1,"modelFingerprint":"digest-or-user-cache-epoch","modelId":"user-selected-model","placeholderVersion":"1","profileHash":"p","promptHash":"q","providerKind":"ollama","requestManifestHash":"m","sourceHash":"sha256-of-sourceText","sourceLanguage":"en","splitterVersion":"1","targetLanguage":"ko"}"#
        );
        assert_eq!(cache_key(&d), crate::domain::sha256_hex(s.as_bytes()));
    }

    /// T15: any single input change => new key.
    #[test]
    fn every_field_changes_key() {
        let k0 = cache_key(&descriptor(&base()));
        let variants: Vec<Box<dyn Fn(&mut DescriptorInput<'static>)>> = vec![
            Box::new(|d| d.source_hash = "x"),
            Box::new(|d| d.context_hash = "x"),
            Box::new(|d| d.model_id = "x"),
            Box::new(|d| d.endpoint_identity = "x"),
            Box::new(|d| d.glossary_hash = "x"),
            Box::new(|d| d.prompt_hash = "x"),
            Box::new(|d| d.profile_hash = "x"),
            Box::new(|d| d.request_manifest_hash = "x"),
            Box::new(|d| d.target_language = "ja"),
            Box::new(|d| d.account_namespace = "x"),
            Box::new(|d| d.model_fingerprint = "x"),
        ];
        for v in variants {
            let mut d = base();
            v(&mut d);
            assert_ne!(cache_key(&descriptor(&d)), k0);
        }
        // Theme/font/mode are not inputs at all => identical key.
        assert_eq!(cache_key(&descriptor(&base())), k0);
    }
}
