//! AWS ECS on Fargate launcher.
//!
//! **Not yet verified in a real AWS account.** It is implemented against the
//! same `Launcher` interface as the process and Docker launchers, and its
//! request planning is unit-tested, but no task has been run on AWS.
//!
//! Each runner is one Fargate task from the per-stack task definition
//! (`<prefix><stack>`), in the runner subnets with the runner security group
//! and no public IP. Only the container environment is overridden. Launches
//! are idempotent: `startedBy` is derived from run and generation, and an
//! existing task with that tag is returned instead of starting another.

use super::*;
use anyhow::{Context, anyhow};
use aws_sdk_ecs::Client;
use aws_sdk_ecs::types::{AssignPublicIp, AwsVpcConfiguration, ContainerOverride, DesiredStatus, KeyValuePair, LaunchType, NetworkConfiguration, TaskOverride};

#[derive(Debug, Clone)]
pub struct FargateConfig {
    pub cluster: String,
    pub subnets: Vec<String>,
    pub security_groups: Vec<String>,
    pub task_definition_prefix: String,
    pub container: String,
}

impl FargateConfig {
    pub fn from_env() -> Option<Self> {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let list = |k: &str| get(k).map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
        Some(FargateConfig {
            cluster: get("FACTORY_FARGATE_CLUSTER")?,
            subnets: list("FACTORY_FARGATE_SUBNETS"),
            security_groups: list("FACTORY_FARGATE_SECURITY_GROUPS"),
            task_definition_prefix: get("FACTORY_FARGATE_TASK_DEFINITION_PREFIX").unwrap_or_else(|| "ai-factory-runner-".into()),
            container: get("FACTORY_FARGATE_CONTAINER").unwrap_or_else(|| "runner".into()),
        })
    }
}

/// Everything a RunTask call carries, computed without AWS for testing.
#[derive(Debug, Clone, PartialEq)]
pub struct RunTaskPlan {
    pub cluster: String,
    pub task_definition: String,
    pub started_by: String,
    pub subnets: Vec<String>,
    pub security_groups: Vec<String>,
    pub container: String,
    pub env: Vec<(String, String)>,
}

/// `startedBy` is limited to 36 characters.
pub fn started_by(spec: &LaunchSpec) -> String {
    let id = spec.run_id.simple().to_string();
    let g = format!("g{}", spec.generation);
    format!("{}{g}", &id[..(36 - g.len()).min(32)])
}

pub fn plan(cfg: &FargateConfig, spec: &LaunchSpec) -> Result<RunTaskPlan> {
    assert_env_clean(&spec.env)?;
    if cfg.subnets.is_empty() {
        anyhow::bail!("FACTORY_FARGATE_SUBNETS is empty");
    }
    Ok(RunTaskPlan {
        cluster: cfg.cluster.clone(),
        task_definition: format!("{}{}", cfg.task_definition_prefix, spec.image),
        started_by: started_by(spec),
        subnets: cfg.subnets.clone(),
        security_groups: cfg.security_groups.clone(),
        container: cfg.container.clone(),
        env: spec.env.clone(),
    })
}

pub struct FargateLauncher {
    cfg: FargateConfig,
    client: Client,
}

impl FargateLauncher {
    pub async fn from_env() -> Option<Self> {
        let cfg = FargateConfig::from_env()?;
        let aws = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        Some(FargateLauncher { cfg, client: Client::new(&aws) })
    }
}

#[async_trait]
impl Launcher for FargateLauncher {
    fn kind(&self) -> &'static str {
        "fargate"
    }

    async fn launch(&self, spec: &LaunchSpec) -> Result<String> {
        let p = plan(&self.cfg, spec)?;
        let existing = self
            .client
            .list_tasks()
            .cluster(&p.cluster)
            .started_by(&p.started_by)
            .desired_status(DesiredStatus::Running)
            .send()
            .await
            .context("ecs:ListTasks")?;
        if let Some(arn) = existing.task_arns().first() {
            return Ok(arn.clone());
        }
        let env: Vec<KeyValuePair> = p.env.iter().map(|(k, v)| KeyValuePair::builder().name(k).value(v).build()).collect();
        let overrides = TaskOverride::builder().container_overrides(ContainerOverride::builder().name(&p.container).set_environment(Some(env)).build()).build();
        let net = NetworkConfiguration::builder()
            .awsvpc_configuration(
                AwsVpcConfiguration::builder()
                    .set_subnets(Some(p.subnets.clone()))
                    .set_security_groups(Some(p.security_groups.clone()))
                    .assign_public_ip(AssignPublicIp::Disabled)
                    .build()
                    .map_err(|e| anyhow!("{e}"))?,
            )
            .build();
        let out = self
            .client
            .run_task()
            .cluster(&p.cluster)
            .task_definition(&p.task_definition)
            .launch_type(LaunchType::Fargate)
            .started_by(&p.started_by)
            .network_configuration(net)
            .overrides(overrides)
            .tags(aws_sdk_ecs::types::Tag::builder().key("factory.run").value(spec.run_id.to_string()).build())
            .send()
            .await
            .context("ecs:RunTask")?;
        if let Some(f) = out.failures().first() {
            anyhow::bail!("RunTask failed: {:?} {:?}", f.reason(), f.detail());
        }
        out.tasks().first().and_then(|t| t.task_arn()).map(String::from).ok_or_else(|| anyhow!("RunTask returned no task"))
    }

    async fn remove(&self, handle: &str) -> Result<()> {
        self.client.stop_task().cluster(&self.cfg.cluster).task(handle).reason("factory teardown").send().await.context("ecs:StopTask")?;
        Ok(())
    }

    async fn alive(&self, handle: &str) -> Result<Option<bool>> {
        let d = self.client.describe_tasks().cluster(&self.cfg.cluster).tasks(handle).send().await.context("ecs:DescribeTasks")?;
        Ok(Some(d.tasks().first().and_then(|t| t.last_status()).map(|s| s == "RUNNING" || s == "PENDING" || s == "PROVISIONING").unwrap_or(false)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plans_run_task() {
        let cfg = FargateConfig { cluster: "c".into(), subnets: vec!["s1".into()], security_groups: vec!["sg".into()], task_definition_prefix: "ai-factory-runner-".into(), container: "runner".into() };
        let spec = LaunchSpec { run_id: Uuid::nil(), generation: 12, name: "x".into(), image: "rust".into(), env: vec![("FACTORY_RUN_TOKEN".into(), "t".into())] };
        let p = plan(&cfg, &spec).unwrap();
        assert_eq!(p.task_definition, "ai-factory-runner-rust");
        assert!(p.started_by.len() <= 36);
        assert!(p.started_by.ends_with("g12"));
        let bad = LaunchSpec { env: vec![("GITHUB_TOKEN".into(), "x".into())], ..spec };
        assert!(plan(&cfg, &bad).is_err(), "connector credentials can never be passed to a runner");
    }
}
