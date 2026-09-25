use settings::{RegisterSetting, Settings};

#[derive(Debug, Clone, PartialEq, RegisterSetting)]
pub struct JjSettings {
    pub saved_revsets: Vec<String>,
}

impl Settings for JjSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let jj = content.jj.clone().unwrap();
        Self {
            saved_revsets: jj.saved_revsets.unwrap(),
        }
    }
}
