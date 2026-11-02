use serde::Deserialize;

fn default_model() -> String { "local-model".into() }
fn default_prompt() -> String { "Count from 1 to 20, one number per line.".into() }
fn default_maxtok_test() -> i64 { 100 }

#[derive(Deserialize, Clone)]
pub struct TestReq {
    #[serde(default = "default_model")]   pub model: String,
    #[serde(default = "default_prompt")]  pub prompt: String,
    #[serde(default = "default_maxtok_test")] pub max_tokens: i64,
    #[serde(default)]                     pub base_url: Option<String>,
}
