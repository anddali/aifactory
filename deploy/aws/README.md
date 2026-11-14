# AWS deployment (ECS on Fargate)

**Status: NOT yet verified in a real AWS account.** These definitions are written against the same interfaces the
local build uses (the `Launcher` trait's Fargate implementation, the plane and egress images). No `terraform plan`
or `apply` has been run against AWS.

- `network.tf`: VPC with public subnets (load balancers, NAT), private subnets (planes, egress proxy, database) and
  **runner subnets with no route to the internet at all**. Runner tasks get a security group whose only egress is to
  the internal plane load balancer, the egress proxy, the ECR/Logs VPC endpoints and (optionally) the LiteLLM proxy
  CIDR. Security groups filter by address; the egress proxy enforces the domain allowlist.
- `database.tf`: RDS for PostgreSQL 17, multi-AZ, encrypted, with secrets in Secrets Manager.
- `plane.tf`: ECS service of N identical plane replicas behind an internet-facing ALB (portal, webhooks) and an
  internal ALB that runners reach as `plane.internal`.
- `egress.tf`: the egress proxy (same image, `egress` entrypoint) behind an internal NLB at `egress.internal:3128`.
- `runners.tf`: one task definition per runner image. The plane's Fargate launcher calls `RunTask` with these,
  overriding only the container environment (run token, plane URLs, run id, generation).
- `iam.tf`: runner tasks have **no task role** (no AWS credentials). The plane's task role may run, describe and stop
  tasks in the cluster and pass the runner execution role.

Plane environment for Fargate (set in `plane.tf`): `FACTORY_FARGATE_CLUSTER`, `FACTORY_FARGATE_SUBNETS`,
`FACTORY_FARGATE_SECURITY_GROUPS`, `FACTORY_FARGATE_TASK_DEFINITION_PREFIX`, `FACTORY_FARGATE_CONTAINER`; then set
the portal setting `launcher` to `fargate`.
