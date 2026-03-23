terraform {
  required_version = ">= 1.5"

  required_providers {
    gitea = {
      source  = "Lerentis/gitea"
      version = "~> 0.16"
    }
    random = {
      source  = "hashicorp/random"
      version = "~> 3.6"
    }
    null = {
      source  = "hashicorp/null"
      version = "~> 3.2"
    }
  }
}
