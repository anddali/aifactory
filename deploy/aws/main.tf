terraform {
  required_version = ">= 1.9"
  required_providers {
    aws = { source = "hashicorp/aws", version = "~> 6.0" }
  }
}

provider "aws" {
  region = var.region
}

variable "region" {
  type    = string
  default = "eu-west-1"
}

variable "name" {
  type    = string
  default = "ai-factory"
}

variable "plane_image" {
  type = string
}

variable "runner_image_prefix" {
  type        = string
  description = "ECR prefix, e.g. 123456789012.dkr.ecr.eu-west-1.amazonaws.com/ai-factory/runner-"
}

variable "plane_replicas" {
  type    = number
  default = 2
}

variable "litellm_cidr" {
  type    = string
  default = ""
}

variable "certificate_arn" {
  type = string
}

variable "entra_tenant_id" {
  type    = string
  default = ""
}

variable "entra_client_id" {
  type    = string
  default = ""
}

data "aws_availability_zones" "available" {
  state = "available"
}

resource "aws_ecs_cluster" "this" {
  name = var.name
  setting {
    name  = "containerInsights"
    value = "enabled"
  }
}

resource "aws_cloudwatch_log_group" "this" {
  name              = "/ecs/${var.name}"
  retention_in_days = 30
}

output "portal_url" {
  value = "https://${aws_lb.public.dns_name}"
}
