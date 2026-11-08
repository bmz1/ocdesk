//! Session preferences, review scopes, and usage. All I/O runs on Tokio.
use super::*;
use crate::settings::PermissionMode;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}
impl Tokens {
    pub fn parse(v: &Value) -> Self {
        Self {
            input: v["input"].as_u64().unwrap_or(0),
            output: v["output"].as_u64().unwrap_or(0),
            reasoning: v["reasoning"].as_u64().unwrap_or(0),
            cache_read: v["cache"]["read"].as_u64().unwrap_or(0),
            cache_write: v["cache"]["write"].as_u64().unwrap_or(0),
        }
    }
    pub fn context(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }
    pub fn total(&self) -> u64 {
        self.context() + self.output
    }
    fn add(&mut self, other: &Self) {
        self.input += other.input;
        self.output += other.output;
        self.reasoning += other.reasoning;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
    }
}
#[derive(Clone, Debug, Default)]
pub struct ModelUsage {
    pub tokens: Tokens,
    pub cost: f64,
}
#[derive(Clone, Debug, Default)]
pub struct UsageReport {
    pub tokens: Tokens,
    pub cost: f64,
    pub models: BTreeMap<String, ModelUsage>,
    pub context: Option<u64>,
    pub context_limit: Option<u64>,
    pub limits: Option<Value>,
    pub limits_error: Option<String>,
}
impl UsageReport {
    /// Add one assistant message's usage to the per-model breakdown.
    pub fn add_model_usage(&mut self, model: &str, tokens: &Tokens, cost: f64) {
        let entry = self.models.entry(model.to_string()).or_default();
        entry.tokens.add(tokens);
        entry.cost += cost;
    }
    fn include_message(&mut self, v: &Value) {
        if v["type"] != "assistant" {
            return;
        }
        let Some(raw) = v.get("tokens").filter(|v| v.is_object()) else {
            return;
        };
        let tokens = Tokens::parse(raw);
        let model = &v["model"];
        let key = format!(
            "{}/{}",
            model["providerID"].as_str().unwrap_or("unknown"),
            model["id"].as_str().unwrap_or("unknown")
        );
        self.add_model_usage(&key, &tokens, v["cost"].as_f64().unwrap_or(0.));
    }
}
impl Api {
    pub async fn session_mode(&self, sid: &str) -> Result<Option<PermissionMode>> {
        let v = self
            .send(self.req(Method::GET, &format!("/api/session/{}", id(sid))))
            .await?;
        Ok(PermissionMode::from_rules(&v["data"]["permissions"]))
    }
    pub async fn set_session_mode(&self, sid: &str, mode: PermissionMode) -> Result<()> {
        self.send(
            self.req(Method::PATCH, &format!("/api/session/{}", id(sid)))
                .json(&json!({"permissions":mode.rules()})),
        )
        .await?;
        Ok(())
    }
    /// Uncommitted changes (HEAD to the working copy, untracked files included).
    pub async fn working_diff(&self, directory: &Path) -> Result<Vec<model::FileDiff>> {
        let req = self.req(Method::GET, "/api/vcs/diff").query(&[
            ("location[directory]", directory.to_string_lossy().as_ref()),
            ("mode", "working"),
            ("context", "3"),
            ("untracked", "files"),
        ]);
        let v = self.send(req).await?;
        v["data"]
            .as_array()
            .ok_or_else(|| anyhow!("invalid VCS diff response"))?
            .iter()
            .map(|v| model::FileDiff::from_json(v).ok_or_else(|| anyhow!("invalid file diff")))
            .collect()
    }
    /// Session totals (one request). With `details`, also the account's
    /// usage limits and, when `history` is set, the per-model breakdown,
    /// context and model limit from the server. Callers leave `history`
    /// off when the conversation is loaded: those are derived from memory
    /// ([`crate::store::Store::usage_view`]) instead of re-downloading it.
    pub async fn usage_report(
        &self,
        sid: &str,
        directory: &Path,
        details: bool,
        history: bool,
    ) -> Result<UsageReport> {
        let root = format!("/api/session/{}", id(sid));
        let v = self.send(self.req(Method::GET, &root)).await?;
        let session = &v["data"];
        let mut report = UsageReport {
            tokens: Tokens::parse(&session["tokens"]),
            cost: session["cost"].as_f64().unwrap_or(0.),
            ..Default::default()
        };
        if !details {
            return Ok(report);
        }
        let location = [(
            "location[directory]",
            directory.to_string_lossy().into_owned(),
        )];
        if history {
            let mut cursor = None::<String>;
            loop {
                let mut q = vec![("limit", PAGE.to_string())];
                if let Some(c) = &cursor {
                    q.push(("cursor", c.clone()));
                } else {
                    q.push(("order", "asc".into()));
                }
                let v = self
                    .send(self.req(Method::GET, &format!("{root}/message")).query(&q))
                    .await?;
                let page = v["data"]
                    .as_array()
                    .ok_or_else(|| anyhow!("invalid usage history"))?;
                for m in page {
                    report.include_message(m);
                }
                let next = v
                    .pointer("/cursor/next")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if page.len() < PAGE || next.is_none() {
                    break;
                }
                if next == cursor {
                    bail!("repeated usage history cursor");
                }
                cursor = next;
            }
            let context = self
                .send(self.req(Method::GET, &format!("{root}/context")))
                .await?;
            let messages = context["data"].as_array();
            report.context = messages.and_then(|ms| {
                ms.iter()
                    .rev()
                    .find(|v| v["type"] == "assistant" && v["tokens"].is_object())
                    .map(|v| Tokens::parse(&v["tokens"]).context())
            });
            let current_model = session
                .get("model")
                .filter(|v| v.is_object())
                .cloned()
                .or_else(|| {
                    messages.and_then(|ms| {
                        ms.iter()
                            .rev()
                            .find_map(|v| v.get("model").filter(|m| m.is_object()).cloned())
                    })
                });
            if let Ok(models) = self
                .send(self.req(Method::GET, "/api/model").query(&location))
                .await
            {
                // The model catalogue is a flat array in opencode 2.x.
                report.context_limit = models["data"]
                    .as_array()
                    .and_then(|ms| {
                        ms.iter().find(|m| {
                            current_model.as_ref().is_some_and(|model| {
                                m["id"] == model["id"] && m["providerID"] == model["providerID"]
                            })
                        })
                    })
                    .and_then(|m| m.pointer("/limit/context"))
                    .and_then(Value::as_u64);
            }
        }
        match self
            .send(
                self.req(Method::GET, "/api/experimental/usage/limits")
                    .query(&location),
            )
            .await
        {
            Ok(v) => report.limits = v.get("data").filter(|v| !v.is_null()).cloned(),
            Err(e) => report.limits_error = Some(format!("{e:#}")),
        }
        Ok(report)
    }
}
pub fn patch_fingerprint(patch: &str) -> u64 {
    patch.bytes().fold(0xcbf29ce484222325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn costs_and_models_do_not_include_non_assistant_entries() {
        let mut r = UsageReport::default();
        let m = json!({"type":"assistant","model":{"providerID":"p","id":"m"},"tokens":{"input":10,"output":20,"reasoning":5,"cache":{"read":30,"write":2}},"cost":0.01});
        r.include_message(&m);
        r.include_message(&json!({"type":"user","tokens":{"input":900}}));
        assert_eq!(r.models["p/m"].tokens.total(), 62);
        assert_eq!(r.models["p/m"].tokens.reasoning, 5);
        assert_eq!(r.models["p/m"].cost, 0.01);
    }
}
#[derive(Clone, Debug)]
pub struct SavedGrant {
    pub id: String,
    pub project_id: String,
    pub action: String,
    pub resource: String,
}
impl Api {
    /// Without projectID, opencode lists only its startup directory's grants.
    /// Resolve every added folder explicitly; worktrees can share a project.
    pub async fn saved_grants(&self, folders: &[PathBuf]) -> Result<Vec<SavedGrant>> {
        let mut projects = BTreeSet::new();
        for folder in folders {
            projects.insert(self.project_id(folder).await?);
        }
        let mut grants = Vec::new();
        for project in projects {
            grants.extend(self.project_saved_grants(&project).await?);
        }
        Ok(grants)
    }

    async fn project_saved_grants(&self, project_id: &str) -> Result<Vec<SavedGrant>> {
        let v = self
            .send(
                self.req(Method::GET, "/api/permission/saved")
                    .query(&[("projectID", project_id)]),
            )
            .await?;
        v["data"]
            .as_array()
            .ok_or_else(|| anyhow!("invalid saved permissions"))?
            .iter()
            .map(|v| {
                Ok(SavedGrant {
                    id: v["id"]
                        .as_str()
                        .ok_or_else(|| anyhow!("missing grant id"))?
                        .into(),
                    project_id: v["projectID"].as_str().unwrap_or_default().into(),
                    action: v["action"].as_str().unwrap_or_default().into(),
                    resource: v["resource"].as_str().unwrap_or_default().into(),
                })
            })
            .collect()
    }
    pub async fn revoke_grant(&self, grant_id: &str) -> Result<()> {
        self.send(self.req(
            Method::DELETE,
            &format!("/api/permission/saved/{}", id(grant_id)),
        ))
        .await?;
        Ok(())
    }
}
