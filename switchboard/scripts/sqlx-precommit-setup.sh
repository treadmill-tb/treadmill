#!/usr/bin/env sh

echo "cargo sqlx prepare > /dev/null 2>&1; git add .sqlx > /dev/null" > .git/hooks/pre-commit