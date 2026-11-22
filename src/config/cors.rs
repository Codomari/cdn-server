use serde::Deserialize;

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct CorsConfig {
    pub(crate) enable: bool,
    pub(crate) allow: CorsAllow,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            enable: true,
            allow: CorsAllow::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct CorsAllow {
    pub(crate) origin: bool,
    pub(crate) methods: bool,
    pub(crate) headers: bool,
    pub(crate) expose_headers: bool,
}

impl Default for CorsAllow {
    fn default() -> Self {
        Self {
            origin: true,
            methods: true,
            headers: true,
            expose_headers: true,
        }
    }
}
