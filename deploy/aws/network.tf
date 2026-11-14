resource "aws_vpc" "this" {
  cidr_block           = "10.40.0.0/16"
  enable_dns_hostnames = true
  tags                 = { Name = var.name }
}

resource "aws_subnet" "public" {
  count                   = 2
  vpc_id                  = aws_vpc.this.id
  cidr_block              = cidrsubnet(aws_vpc.this.cidr_block, 8, count.index)
  availability_zone       = data.aws_availability_zones.available.names[count.index]
  map_public_ip_on_launch = true
}

resource "aws_subnet" "private" {
  count             = 2
  vpc_id            = aws_vpc.this.id
  cidr_block        = cidrsubnet(aws_vpc.this.cidr_block, 8, 10 + count.index)
  availability_zone = data.aws_availability_zones.available.names[count.index]
}

# Runner subnets: no NAT and no internet gateway route.
resource "aws_subnet" "runners" {
  count             = 2
  vpc_id            = aws_vpc.this.id
  cidr_block        = cidrsubnet(aws_vpc.this.cidr_block, 8, 20 + count.index)
  availability_zone = data.aws_availability_zones.available.names[count.index]
}

resource "aws_internet_gateway" "this" {
  vpc_id = aws_vpc.this.id
}

resource "aws_eip" "nat" {
  domain = "vpc"
}

resource "aws_nat_gateway" "this" {
  allocation_id = aws_eip.nat.id
  subnet_id     = aws_subnet.public[0].id
}

resource "aws_route_table" "public" {
  vpc_id = aws_vpc.this.id
  route {
    cidr_block = "0.0.0.0/0"
    gateway_id = aws_internet_gateway.this.id
  }
}

resource "aws_route_table" "private" {
  vpc_id = aws_vpc.this.id
  route {
    cidr_block     = "0.0.0.0/0"
    nat_gateway_id = aws_nat_gateway.this.id
  }
}

resource "aws_route_table" "runners" {
  vpc_id = aws_vpc.this.id
}

resource "aws_route_table_association" "public" {
  count          = 2
  subnet_id      = aws_subnet.public[count.index].id
  route_table_id = aws_route_table.public.id
}

resource "aws_route_table_association" "private" {
  count          = 2
  subnet_id      = aws_subnet.private[count.index].id
  route_table_id = aws_route_table.private.id
}

resource "aws_route_table_association" "runners" {
  count          = 2
  subnet_id      = aws_subnet.runners[count.index].id
  route_table_id = aws_route_table.runners.id
}

# Image pulls and logs for runners without internet access.
resource "aws_vpc_endpoint" "interface" {
  for_each            = toset(["ecr.api", "ecr.dkr", "logs"])
  vpc_id              = aws_vpc.this.id
  service_name        = "com.amazonaws.${var.region}.${each.key}"
  vpc_endpoint_type   = "Interface"
  subnet_ids          = aws_subnet.runners[*].id
  security_group_ids  = [aws_security_group.endpoints.id]
  private_dns_enabled = true
}

resource "aws_vpc_endpoint" "s3" {
  vpc_id            = aws_vpc.this.id
  service_name      = "com.amazonaws.${var.region}.s3"
  vpc_endpoint_type = "Gateway"
  route_table_ids   = [aws_route_table.runners.id]
}

resource "aws_security_group" "endpoints" {
  name   = "${var.name}-endpoints"
  vpc_id = aws_vpc.this.id
}

resource "aws_security_group" "public_alb" {
  name   = "${var.name}-public-alb"
  vpc_id = aws_vpc.this.id
}

resource "aws_security_group" "internal_alb" {
  name   = "${var.name}-internal-alb"
  vpc_id = aws_vpc.this.id
}

resource "aws_security_group" "plane" {
  name   = "${var.name}-plane"
  vpc_id = aws_vpc.this.id
}

resource "aws_security_group" "egress_proxy" {
  name   = "${var.name}-egress-proxy"
  vpc_id = aws_vpc.this.id
}

# Runners: no inbound; outbound only to the plane, the egress proxy, the VPC
# endpoints and optionally LiteLLM.
resource "aws_security_group" "runners" {
  name   = "${var.name}-runners"
  vpc_id = aws_vpc.this.id
}

locals {
  sg_rules = {
    public_alb_in_https     = { sg = "public_alb", dir = "in", port = 443, cidr = "0.0.0.0/0" }
    public_alb_to_plane     = { sg = "public_alb", dir = "out", port = 8080, peer = "plane" }
    internal_alb_from_runner = { sg = "internal_alb", dir = "in", port = 443, peer = "runners" }
    internal_alb_to_plane   = { sg = "internal_alb", dir = "out", port = 8080, peer = "plane" }
    plane_from_public_alb   = { sg = "plane", dir = "in", port = 8080, peer = "public_alb" }
    plane_from_internal_alb = { sg = "plane", dir = "in", port = 8080, peer = "internal_alb" }
    plane_out_any           = { sg = "plane", dir = "out", port = -1, cidr = "0.0.0.0/0" }
    egress_from_runners     = { sg = "egress_proxy", dir = "in", port = 3128, peer = "runners" }
    egress_out_https        = { sg = "egress_proxy", dir = "out", port = 443, cidr = "0.0.0.0/0" }
    egress_out_http         = { sg = "egress_proxy", dir = "out", port = 80, cidr = "0.0.0.0/0" }
    egress_out_db           = { sg = "egress_proxy", dir = "out", port = 5432, cidr = "10.40.0.0/16" }
    endpoints_from_runners  = { sg = "endpoints", dir = "in", port = 443, peer = "runners" }
    runners_to_plane        = { sg = "runners", dir = "out", port = 443, peer = "internal_alb" }
    runners_to_egress       = { sg = "runners", dir = "out", port = 3128, peer = "egress_proxy" }
    runners_to_endpoints    = { sg = "runners", dir = "out", port = 443, peer = "endpoints" }
  }
  sgs = {
    public_alb   = aws_security_group.public_alb.id
    internal_alb = aws_security_group.internal_alb.id
    plane        = aws_security_group.plane.id
    egress_proxy = aws_security_group.egress_proxy.id
    endpoints    = aws_security_group.endpoints.id
    runners      = aws_security_group.runners.id
  }
}

resource "aws_vpc_security_group_ingress_rule" "rules" {
  for_each                     = { for k, v in local.sg_rules : k => v if v.dir == "in" }
  security_group_id            = local.sgs[each.value.sg]
  ip_protocol                  = "tcp"
  from_port                    = each.value.port
  to_port                      = each.value.port
  cidr_ipv4                    = lookup(each.value, "cidr", null)
  referenced_security_group_id = contains(keys(each.value), "peer") ? local.sgs[each.value.peer] : null
}

resource "aws_vpc_security_group_egress_rule" "rules" {
  for_each                     = { for k, v in local.sg_rules : k => v if v.dir == "out" }
  security_group_id            = local.sgs[each.value.sg]
  ip_protocol                  = each.value.port == -1 ? "-1" : "tcp"
  from_port                    = each.value.port == -1 ? null : each.value.port
  to_port                      = each.value.port == -1 ? null : each.value.port
  cidr_ipv4                    = lookup(each.value, "cidr", null)
  referenced_security_group_id = contains(keys(each.value), "peer") ? local.sgs[each.value.peer] : null
}

resource "aws_vpc_security_group_egress_rule" "runners_to_litellm" {
  count             = var.litellm_cidr == "" ? 0 : 1
  security_group_id = aws_security_group.runners.id
  cidr_ipv4         = var.litellm_cidr
  ip_protocol       = "tcp"
  from_port         = 443
  to_port           = 443
}
