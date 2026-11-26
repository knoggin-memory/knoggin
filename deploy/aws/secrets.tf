# Tab "Deployment", AWS: the router's four secrets. The operator fills the Secrets Manager
# entries by hand (deploy-aws.md), so no value ever sits in the OpenTofu state. The
# router's execution role reads them and ECS hands them to the router as environment
# variables, under the names the config loader reads (tab "Config, CLI, local install").

locals {
  router_secrets = {
    KNOGGIN_AUTH__WEB_CLIENT_SECRET = var.auth_web_client_secret_arn
    KNOGGIN_INTERNAL__SECRET        = var.internal_secret_arn
    KNOGGIN_INTERNAL__CERT          = var.internal_cert_arn
    KNOGGIN_INTERNAL__KEY           = var.internal_key_arn
  }
}
