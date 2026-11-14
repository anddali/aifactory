# Database, IAM, plane, egress proxy and runner task definitions.

resource "aws_db_subnet_group" "this" {
  name       = var.name
  subnet_ids = aws_subnet.private[*].id
}

resource "aws_security_group" "db" {
  name   = "${var.name}-db"
  vpc_id = aws_vpc.this.id
  ingress {
    from_port       = 5432
    to_port         = 5432
    protocol        = "tcp"
    security_groups = [aws_security_group.plane.id, aws_security_group.egress_proxy.id]
  }
}

resource "aws_db_instance" "this" {
  identifier                  = var.name
  engine                      = "postgres"
  engine_version              = "17"
  instance_class              = "db.m7g.large"
  allocated_storage           = 100
  storage_encrypted           = true
  multi_az                    = true
  db_name                     = "factory"
  username                    = "factory"
  manage_master_user_password = true
  db_subnet_group_name        = aws_db_subnet_group.this.name
  vpc_security_group_ids      = [aws_security_group.db.id]
  backup_retention_period     = 14
  deletion_protection         = true
  final_snapshot_identifier   = "${var.name}-final"
}

# Values are set out of band (DATABASE_URL built from the RDS secret; a 32-byte key).
resource "aws_secretsmanager_secret" "database_url" {
  name = "${var.name}/database-url"
}

resource "aws_secretsmanager_secret" "encryption_keys" {
  name = "${var.name}/encryption-keys"
}

data "aws_iam_policy_document" "ecs_assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["ecs-tasks.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "execution" {
  name               = "${var.name}-execution"
  assume_role_policy = data.aws_iam_policy_document.ecs_assume.json
}

resource "aws_iam_role_policy_attachment" "execution" {
  role       = aws_iam_role.execution.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AmazonECSTaskExecutionRolePolicy"
}

resource "aws_iam_role_policy" "execution_secrets" {
  role = aws_iam_role.execution.id
  policy = jsonencode({
    Version   = "2012-10-17"
    Statement = [{ Effect = "Allow", Action = ["secretsmanager:GetSecretValue"], Resource = [aws_secretsmanager_secret.database_url.arn, aws_secretsmanager_secret.encryption_keys.arn] }]
  })
}

# Runner tasks: execution role only. No task role, so no AWS credentials inside a runner.
resource "aws_iam_role" "runner_execution" {
  name               = "${var.name}-runner-execution"
  assume_role_policy = data.aws_iam_policy_document.ecs_assume.json
}

resource "aws_iam_role_policy_attachment" "runner_execution" {
  role       = aws_iam_role.runner_execution.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AmazonECSTaskExecutionRolePolicy"
}

resource "aws_iam_role" "plane_task" {
  name               = "${var.name}-plane-task"
  assume_role_policy = data.aws_iam_policy_document.ecs_assume.json
}

resource "aws_iam_role_policy" "plane_runs_runners" {
  role = aws_iam_role.plane_task.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      { Effect = "Allow", Action = ["ecs:RunTask", "ecs:StopTask", "ecs:DescribeTasks", "ecs:ListTasks", "ecs:TagResource"], Resource = "*", Condition = { ArnEquals = { "ecs:cluster" = aws_ecs_cluster.this.arn } } },
      { Effect = "Allow", Action = ["iam:PassRole"], Resource = [aws_iam_role.runner_execution.arn] }
    ]
  })
}

locals {
  log = { logDriver = "awslogs", options = { awslogs-group = aws_cloudwatch_log_group.this.name, awslogs-region = var.region } }
  secrets = [
    { name = "DATABASE_URL", valueFrom = aws_secretsmanager_secret.database_url.arn },
    { name = "FACTORY_ENCRYPTION_KEYS", valueFrom = aws_secretsmanager_secret.encryption_keys.arn },
  ]
}

resource "aws_ecs_task_definition" "plane" {
  family                   = "${var.name}-plane"
  requires_compatibilities = ["FARGATE"]
  network_mode             = "awsvpc"
  cpu                      = 1024
  memory                   = 2048
  execution_role_arn       = aws_iam_role.execution.arn
  task_role_arn            = aws_iam_role.plane_task.arn
  container_definitions = jsonencode([{
    name         = "plane"
    image        = var.plane_image
    essential    = true
    portMappings = [{ containerPort = 8080 }]
    environment = [
      { name = "FACTORY_PUBLIC_URLS", value = "https://plane.internal" },
      { name = "FACTORY_FARGATE_CLUSTER", value = aws_ecs_cluster.this.name },
      { name = "FACTORY_FARGATE_SUBNETS", value = join(",", aws_subnet.runners[*].id) },
      { name = "FACTORY_FARGATE_SECURITY_GROUPS", value = aws_security_group.runners.id },
      { name = "FACTORY_FARGATE_TASK_DEFINITION_PREFIX", value = "${var.name}-runner-" },
      { name = "FACTORY_FARGATE_CONTAINER", value = "runner" },
      { name = "FACTORY_ENTRA_TENANT_ID", value = var.entra_tenant_id },
      { name = "FACTORY_ENTRA_CLIENT_ID", value = var.entra_client_id },
    ]
    secrets          = local.secrets
    healthCheck      = { command = ["CMD-SHELL", "curl -fsS http://127.0.0.1:8080/healthz || exit 1"], interval = 10, retries = 3 }
    logConfiguration = merge(local.log, { options = merge(local.log.options, { awslogs-stream-prefix = "plane" }) })
  }])
}

resource "aws_lb" "public" {
  name               = "${var.name}-public"
  load_balancer_type = "application"
  subnets            = aws_subnet.public[*].id
  security_groups    = [aws_security_group.public_alb.id]
}

resource "aws_lb" "internal" {
  name               = "${var.name}-internal"
  internal           = true
  load_balancer_type = "application"
  subnets            = aws_subnet.private[*].id
  security_groups    = [aws_security_group.internal_alb.id]
  idle_timeout       = 120 # runner long-polls
}

resource "aws_lb_target_group" "plane" {
  name                 = "${var.name}-plane"
  port                 = 8080
  protocol             = "HTTP"
  vpc_id               = aws_vpc.this.id
  target_type          = "ip"
  deregistration_delay = 60
  health_check {
    path = "/readyz"
  }
}

resource "aws_lb_listener" "public" {
  load_balancer_arn = aws_lb.public.arn
  port              = 443
  protocol          = "HTTPS"
  certificate_arn   = var.certificate_arn
  default_action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.plane.arn
  }
}

resource "aws_lb_listener" "internal" {
  load_balancer_arn = aws_lb.internal.arn
  port              = 443
  protocol          = "HTTPS"
  certificate_arn   = var.certificate_arn
  default_action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.plane.arn
  }
}

resource "aws_ecs_service" "plane" {
  name                               = "${var.name}-plane"
  cluster                            = aws_ecs_cluster.this.id
  task_definition                    = aws_ecs_task_definition.plane.arn
  desired_count                      = var.plane_replicas
  launch_type                        = "FARGATE"
  deployment_minimum_healthy_percent = 50
  deployment_maximum_percent         = 200
  network_configuration {
    subnets         = aws_subnet.private[*].id
    security_groups = [aws_security_group.plane.id]
  }
  load_balancer {
    target_group_arn = aws_lb_target_group.plane.arn
    container_name   = "plane"
    container_port   = 8080
  }
}

resource "aws_ecs_task_definition" "egress" {
  family                   = "${var.name}-egress"
  requires_compatibilities = ["FARGATE"]
  network_mode             = "awsvpc"
  cpu                      = 512
  memory                   = 1024
  execution_role_arn       = aws_iam_role.execution.arn
  container_definitions = jsonencode([{
    name             = "egress"
    image            = var.plane_image
    entryPoint       = ["/usr/local/bin/egress"]
    essential        = true
    portMappings     = [{ containerPort = 3128 }]
    environment      = [{ name = "FACTORY_EGRESS_LISTEN", value = "0.0.0.0:3128" }]
    secrets          = local.secrets
    logConfiguration = merge(local.log, { options = merge(local.log.options, { awslogs-stream-prefix = "egress" }) })
  }])
}

resource "aws_lb" "egress" {
  name               = "${var.name}-egress"
  internal           = true
  load_balancer_type = "network"
  subnets            = aws_subnet.private[*].id
  security_groups    = [aws_security_group.egress_proxy.id]
}

resource "aws_lb_target_group" "egress" {
  name        = "${var.name}-egress"
  port        = 3128
  protocol    = "TCP"
  vpc_id      = aws_vpc.this.id
  target_type = "ip"
}

resource "aws_lb_listener" "egress" {
  load_balancer_arn = aws_lb.egress.arn
  port              = 3128
  protocol          = "TCP"
  default_action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.egress.arn
  }
}

resource "aws_ecs_service" "egress" {
  name            = "${var.name}-egress"
  cluster         = aws_ecs_cluster.this.id
  task_definition = aws_ecs_task_definition.egress.arn
  desired_count   = 2
  launch_type     = "FARGATE"
  network_configuration {
    subnets         = aws_subnet.private[*].id
    security_groups = [aws_security_group.egress_proxy.id]
  }
  load_balancer {
    target_group_arn = aws_lb_target_group.egress.arn
    container_name   = "egress"
    container_port   = 3128
  }
}

resource "aws_route53_zone" "internal" {
  name = "internal"
  vpc {
    vpc_id = aws_vpc.this.id
  }
}

resource "aws_route53_record" "plane" {
  zone_id = aws_route53_zone.internal.zone_id
  name    = "plane.internal"
  type    = "A"
  alias {
    name                   = aws_lb.internal.dns_name
    zone_id                = aws_lb.internal.zone_id
    evaluate_target_health = true
  }
}

resource "aws_route53_record" "egress" {
  zone_id = aws_route53_zone.internal.zone_id
  name    = "egress.internal"
  type    = "A"
  alias {
    name                   = aws_lb.egress.dns_name
    zone_id                = aws_lb.egress.zone_id
    evaluate_target_health = true
  }
}

# One task definition per runner image; the Fargate launcher overrides only the environment.
resource "aws_ecs_task_definition" "runner" {
  for_each                 = toset(["python", "dotnet", "node", "rust", "polyglot"])
  family                   = "${var.name}-runner-${each.key}"
  requires_compatibilities = ["FARGATE"]
  network_mode             = "awsvpc"
  cpu                      = 2048
  memory                   = 8192
  execution_role_arn       = aws_iam_role.runner_execution.arn
  ephemeral_storage {
    size_in_gib = 50
  }
  container_definitions = jsonencode([{
    name      = "runner"
    image     = "${var.runner_image_prefix}${each.key}:latest"
    essential = true
    environment = [
      { name = "HTTPS_PROXY", value = "http://egress.internal:3128" },
      { name = "HTTP_PROXY", value = "http://egress.internal:3128" },
      { name = "NO_PROXY", value = "plane.internal" },
    ]
    logConfiguration = merge(local.log, { options = merge(local.log.options, { awslogs-stream-prefix = "runner-${each.key}" }) })
  }])
}
