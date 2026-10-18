//! `<prefix>_<32 lowercase hex>` IDs, as in `packages/core/src/ids.ts`.

macro_rules! prefixes {
    ($($variant:ident => $text:literal),+ $(,)?) => {
        /// An ID prefix. The order matches `idPrefixes` in `packages/core/src/ids.ts`.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum IdPrefix { $($variant),+ }

        impl IdPrefix {
            pub const ALL: &[IdPrefix] = &[$(IdPrefix::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self { $(IdPrefix::$variant => $text),+ }
            }
        }
    };
}

prefixes! {
    Prj => "prj", Mis => "mis", Dir => "dir", Ar => "ar", Exp => "exp", Run => "run", Job => "job",
    Trm => "trm", Tsk => "tsk", Rev => "rev", Art => "art", Evd => "evd", Clm => "clm", Agt => "agt",
    Hnd => "hnd", Blk => "blk", Rsp => "rsp", Evt => "evt", Cmd => "cmd", Snap => "snap", Thr => "thr",
    Epi => "epi", Skl => "skl", Prg => "prg", Ins => "ins", Ivn => "ivn", Wrk => "wrk",
}

/// A new random ID: the prefix and a v4 UUID without dashes.
pub fn create_id(prefix: IdPrefix) -> String {
    format!("{}_{}", prefix.as_str(), random_hex())
}

/// `crypto.randomUUID().replaceAll("-", "")`: a v4 UUID as 32 lowercase hex digits.
pub fn random_hex() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Whether `value` is a NOSH ID, optionally with a specific prefix.
pub fn is_nosh_id(value: &str, prefix: Option<IdPrefix>) -> bool {
    let Some((head, hex)) = value.split_once('_') else {
        return false;
    };
    let hex_ok = hex.len() == 32 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    hex_ok
        && match prefix {
            Some(prefix) => head == prefix.as_str(),
            None => IdPrefix::ALL.iter().any(|p| p.as_str() == head),
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn created_ids_are_recognised() {
        for &prefix in IdPrefix::ALL {
            let id = create_id(prefix);
            assert!(is_nosh_id(&id, Some(prefix)), "{id}");
            assert!(is_nosh_id(&id, None), "{id}");
        }
        assert_ne!(create_id(IdPrefix::Evt), create_id(IdPrefix::Evt));
    }

    #[test]
    fn rejects_malformed_ids() {
        let hex = "0123456789abcdef0123456789abcdef";
        assert!(is_nosh_id(&format!("prj_{hex}"), Some(IdPrefix::Prj)));
        assert!(!is_nosh_id(&format!("prj_{hex}"), Some(IdPrefix::Mis)));
        assert!(!is_nosh_id(&format!("xyz_{hex}"), None));
        assert!(!is_nosh_id(&format!("prj_{}", hex.to_uppercase()), None));
        assert!(!is_nosh_id(&format!("prj_{hex}0"), None));
        assert!(!is_nosh_id(&format!("prj_{hex}\n"), None));
        assert!(!is_nosh_id(&format!("prj{hex}"), None));
    }

    /// The prefix list must match the TypeScript source; a new prefix there fails here.
    #[test]
    fn prefixes_match_typescript() {
        let path = crate::registry::ts_root().join("packages/core/src/ids.ts");
        let source =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let list = source
            .split("idPrefixes = [")
            .nth(1)
            .and_then(|rest| rest.split(']').next())
            .expect("idPrefixes array");
        let theirs: Vec<&str> = list
            .split(',')
            .map(|s| s.trim().trim_matches('"'))
            .filter(|s| !s.is_empty())
            .collect();
        let ours: Vec<&str> = IdPrefix::ALL.iter().map(|p| p.as_str()).collect();
        assert_eq!(ours, theirs);
    }
}
