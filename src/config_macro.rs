#[allow(unused_imports)]
use std::collections::BTreeMap;

/// Metadata for a single config field.
pub struct FieldMeta {
    pub name: &'static str,
    pub path: &'static str,
    pub ty: &'static str,
    pub default: serde_json::Value,
    pub min: Option<serde_json::Value>,
    pub max: Option<serde_json::Value>,
    pub allowed: &'static [&'static str],
    pub desc: &'static str,
}

/// Trait for custom normalization. Called after `clamp()` during both
/// initial load and hot-reload. Implement manually per config struct.
pub trait Normalizable {
    fn normalize(&mut self) {}
}

/// Define config structs with embedded metadata.
///
/// Constraint helpers:
/// - `range(min, max)` — numeric range clamp
/// - `str_max(n)` — String max length (truncation)
/// - `allowed(["val1", "val2"])` — whitelist (fallback to default)
/// - `allowed_max(["val1", "val2"], n)` — whitelist + max length
/// - `none` — no constraint
#[macro_export]
macro_rules! define_config {
    (
        $section:ident => $name:ident {
            $(
                $field:ident : $fty:ty = $def:expr,
                $cname:tt $( ($($args:tt)+) )? ,
                desc: $desc:expr,
            )+
        }
    ) => {
        $crate::_cfg_mixed_expand!(
            @sec $section, $name,
            [ $( ($field, $fty, $def, $cname $( ($($args)+) )? , $desc) )+ ]
        );
    };
}

// ── Mixed-constraint expander ──

#[doc(hidden)]
#[macro_export]
macro_rules! _cfg_mixed_expand {
    (@sec $section:ident, $name:ident,
     [ $( ($field:ident, $fty:ty, $def:expr, $cname:tt $( ($($args:tt)+) )? , $desc:expr) )+ ]) => {
        #[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
        #[serde(default)]
        pub struct $name {
            $( pub $field: $fty, )+
            #[serde(flatten)]
            pub unknown: serde_json::Map<String, serde_json::Value>,
        }
        impl Default for $name {
            fn default() -> Self {
                Self { $( $field: $def, )+ unknown: Default::default() }
            }
        }
        impl $name {
            pub fn fields() -> &'static [$crate::config_macro::FieldMeta] {
                Box::leak(Box::new([
                    $( $crate::_cfg_field_meta!(
                        $section, $field, $fty, $def, $cname $( ($($args)+) )? , $desc
                    ) ),+
                ]))
            }
            pub fn defaults() -> &'static BTreeMap<&'static str, serde_json::Value> {
                use std::sync::OnceLock;
                static CACHE: OnceLock<BTreeMap<&'static str, serde_json::Value>> = OnceLock::new();
                CACHE.get_or_init(|| BTreeMap::from_iter([$( (stringify!($field), serde_json::json!($def)), )+]))
            }
            pub fn clamp(&mut self) {
                let s = &mut *self;
                $(
                    $crate::_clamp_gen!(s.$field, $fty, $def, $cname $( ($($args)+) )?);
                )+
            }
        }
    };
}

// ── Clamp code generation ──
// Uses $self:expr to access struct fields via the local `s` binding.

#[doc(hidden)]
#[macro_export]
macro_rules! _clamp_gen {
    // range: numeric clamp
    ($self:expr, $fty:ty, $def:expr, range($min:tt, $max:tt)) => {
        $self = $self.clamp($min as _, $max as _);
    };
    // str_max: String truncate
    ($self:expr, $fty:ty, $def:expr, str_max($max:tt)) => {
        if !$self.is_empty() {
            let max_len = $max as usize;
            if $self.len() > max_len { $self.truncate(max_len); }
        }
    };
    // allowed: String whitelist fallback
    ($self:expr, $fty:ty, $def:expr, allowed([$($v:tt),+])) => {
        if !$self.is_empty() && ![$($v),+].contains(&$self.as_str()) {
            $self = $def.into();
        }
    };
    // allowed_max: String whitelist + max length
    ($self:expr, $fty:ty, $def:expr, allowed_max([$($v:tt),+], $mx:tt)) => {
        if !$self.is_empty() {
            let max_len = $mx as usize;
            if $self.len() > max_len { $self.truncate(max_len); }
            if ![$($v),+].contains(&$self.as_str()) {
                $self = $def.into();
            }
        }
    };
    // none: no constraint
    ($self:expr, $fty:ty, $def:expr, none) => {};
}

// ── FieldMeta generator ──

#[doc(hidden)]
#[macro_export]
macro_rules! _cfg_field_meta {
    ($section:ident, $field:ident, $fty:ty, $def:expr, $c:tt($($a:tt)+), $desc:expr) => {
        $crate::_cfg_field_meta!(@dispatch $c, $section, $field, $fty, $def, $($a)+, $desc)
    };
    ($section:ident, $field:ident, $fty:ty, $def:expr, none, $desc:expr) => {
        $crate::config_macro::FieldMeta {
            name: stringify!($field),
            path: concat!(stringify!($section), ".", stringify!($field)),
            ty: stringify!($fty),
            default: serde_json::json!($def),
            min: None, max: None, allowed: &[], desc: $desc,
        }
    };

    (@dispatch $kind:tt, $section:ident, $field:ident, $fty:ty, $def:expr, $min:tt, $max:tt, $desc:expr) => {
        $crate::config_macro::FieldMeta {
            name: stringify!($field),
            path: concat!(stringify!($section), ".", stringify!($field)),
            ty: stringify!($fty),
            default: serde_json::json!($def),
            min: Some(serde_json::json!($min)),
            max: Some(serde_json::json!($max)), allowed: &[], desc: $desc,
        }
    };
    (@dispatch $kind:tt, $section:ident, $field:ident, $fty:ty, $def:expr, $max:tt, $desc:expr) => {
        $crate::_cfg_field_meta!(@str_dispatch $kind, $section, $field, $fty, $def, $max, $desc)
    };

    (@str_dispatch str_max, $section:ident, $field:ident, $fty:ty, $def:expr, $max:tt, $desc:expr) => {
        $crate::config_macro::FieldMeta {
            name: stringify!($field),
            path: concat!(stringify!($section), ".", stringify!($field)),
            ty: stringify!($fty),
            default: serde_json::json!($def),
            min: None, max: Some(serde_json::json!($max)), allowed: &[], desc: $desc,
        }
    };
    (@str_dispatch allowed, $section:ident, $field:ident, $fty:ty, $def:expr, [$($v:tt),+], $desc:expr) => {
        $crate::config_macro::FieldMeta {
            name: stringify!($field),
            path: concat!(stringify!($section), ".", stringify!($field)),
            ty: stringify!($fty),
            default: serde_json::json!($def),
            min: None, max: None, allowed: &[$($v),+], desc: $desc,
        }
    };
    (@str_dispatch allowed_max, $section:ident, $field:ident, $fty:ty, $def:expr, [$($v:tt),+], $mx:tt, $desc:expr) => {
        $crate::config_macro::FieldMeta {
            name: stringify!($field),
            path: concat!(stringify!($section), ".", stringify!($field)),
            ty: stringify!($fty),
            default: serde_json::json!($def),
            min: None, max: Some(serde_json::json!($mx)), allowed: &[$($v),+], desc: $desc,
        }
    };
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;

    define_config! {
        test => TestConfig {
            count: u32 = 10, range(1, 100), desc: "Test count",
            ratio: f32 = 0.5, range(0.0, 1.0), desc: "Test ratio",
            name: String = "default".to_string(), str_max(32), desc: "Test name",
            enabled: bool = true, none, desc: "Test enabled",
            kind: String = "a".to_string(), allowed(["a", "b", "c"]), desc: "Test kind",
        }
    }

    #[test]
    fn test_struct_has_all_fields() {
        let c = TestConfig::default();
        assert_eq!(c.count, 10);
        assert!((c.ratio - 0.5).abs() < f32::EPSILON);
        assert_eq!(c.name, "default");
        assert!(c.enabled);
        assert_eq!(c.kind, "a");
    }

    #[test]
    fn test_schema_has_all_metas() {
        let metas = TestConfig::fields();
        assert_eq!(metas.len(), 5);
        let count_meta = metas.iter().find(|m| m.name == "count").unwrap();
        assert_eq!(count_meta.path, "test.count");
        assert_eq!(count_meta.ty, "u32");
        assert_eq!(count_meta.desc, "Test count");
    }

    #[test]
    fn test_defaults_returns_all() {
        let defaults = TestConfig::defaults();
        assert!(defaults.contains_key("count"));
        assert!(defaults.contains_key("ratio"));
        assert!(defaults.contains_key("name"));
        assert!(defaults.contains_key("enabled"));
        assert!(defaults.contains_key("kind"));
        assert_eq!(defaults["count"], serde_json::json!(10));
    }

    #[test]
    fn test_clamp_numeric() {
        let mut c = TestConfig {
            count: 999,
            ratio: 5.0,
            name: "ok".to_string(),
            enabled: true,
            kind: "a".to_string(),
            unknown: Default::default(),
        };
        c.clamp();
        assert_eq!(c.count, 100);
        assert!((c.ratio - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_clamp_string_allowed_fallback() {
        let mut c = TestConfig {
            count: 10,
            ratio: 0.5,
            name: "ok".to_string(),
            enabled: true,
            kind: "invalid".to_string(),
            unknown: Default::default(),
        };
        c.clamp();
        assert_eq!(c.kind, "a");
    }

    #[test]
    fn test_clamp_string_max_truncate() {
        let mut c = TestConfig {
            count: 10,
            ratio: 0.5,
            name: "this is a very long name that exceeds 32 chars".to_string(),
            enabled: true,
            kind: "a".to_string(),
            unknown: Default::default(),
        };
        c.clamp();
        assert!(c.name.len() <= 32);
    }

    #[test]
    fn test_unknown_fields_roundtrip() {
        let json = r#"{
            "count": 10,
            "ratio": 0.5,
            "name": "test",
            "enabled": true,
            "kind": "a",
            "future_field": "hello",
            "another": 42
        }"#;
        let c: TestConfig = serde_json::from_str(json).unwrap();
        assert!(c.unknown.contains_key("future_field"));
        assert!(c.unknown.contains_key("another"));
        let out = serde_json::to_string(&c).unwrap();
        assert!(out.contains("future_field"));
    }
}
