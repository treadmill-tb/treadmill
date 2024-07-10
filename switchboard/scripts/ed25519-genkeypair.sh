#!/usr/bin/env sh

if [[ $# -ne 2 ]] ; then
  echo "ed25519-genkeypair.sh <PREFIX>"
  echo ""
  echo "This command will generate an ed25519 keypair at <PREFIX>.pem, <PREFIX>-public.pem."
  echo "Any nonexistent directories in the <PREFIX> path will be generated automatically."
  exit 1
fi

PREFIX=$1

mkdir -p $(dirname "${PREFIX}")

PRIVKEY="$1.pem"
PUBKEY="$1-public.pem"

openssl genpkey -algorithm ed25519 -outform PEM -out "${PRIVKEY}"
openssl pkey -in "${PRIVKEY}" -pubout -outform PEM -out "${PUBKEY}"

echo "-- ed25519-genkeypair.sh --"
echo "Generated ed25519 private key at ${PRIVKEY}"
echo "Generated ed25519 public key at ${PUBKEY}"
