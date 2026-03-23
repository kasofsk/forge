#!/bin/sh
# Git credential helper — returns FORGEJO_TOKEN for HTTP cloning
echo "username=token"
echo "password=${FORGEJO_TOKEN}"
