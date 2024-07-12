#!/usr/bin/env sh

if [ $# -ne 2 ] ; then
  echo "setup-tls-certs.sh <DIR> <HOST>"
  echo ""
  echo "This command will generate a TLS keypair for <HOST> signed by a local root CA at <DIR>/<PREFIX>.pem, <DIR>/<PREFIX>-public.pem."
  echo "Any nonexistent directories in the <DIR> path will be generated automatically."
  exit 1
fi

DIR=$1
HOST=$2

mkdir -p "${DIR}"

PRIVKEY="${DIR}/${HOST}.pem"
PUBKEY="${DIR}/${HOST}-public.pem"

if [ -f "${PRIVKEY}" ] ; then
  echo "private key already exists: deleting"
  rm "${PRIVKEY}"
fi
if [ -f "${PUBKEY}" ] ; then
  echo "public key already exists: deleting"
  rm "${PUBKEY}"
fi

mkcert -install
mkcert -cert-file "${PUBKEY}" -key-file "${PRIVKEY}" localhost

echo "-- setup-tls-certs.sh --"
echo "Generated TLS private key at ${PRIVKEY}"
echo "Generated TLS public key at ${PUBKEY}"
