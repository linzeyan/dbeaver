#!/bin/sh
# A CA, a server certificate it signed, and an HBA file, for the TLS test
# server. Written to $1.
#
# Three things here are load bearing:
#
#   * The HBA file has only `hostssl` lines, so a connection asking for no
#     encryption is turned away. Without that, "it connected" in the TLS tests
#     would say nothing about whether TLS was used at all.
#   * The name on the server certificate is not the address anything connects
#     to. That is the only difference between `verify-ca`, which must accept the
#     chain anyway, and `verify-full`, which must refuse it.
#   * The server certificate is signed by a separate CA rather than being
#     self-signed. rustls refuses a certificate marked `CA:TRUE` when it arrives
#     as the leaf — correctly — so a self-signed one would fail for a reason
#     that has nothing to do with the mode being tested.
#
# Regenerating is skipped once the CA is there: the running container holds a
# copy of the server certificate from when it was created, and issuing a new one
# underneath it would leave the tests trusting one file and being shown another.
set -eu

out="${1:?usage: make-pgtls-certs.sh <directory>}"
mkdir -p "$out"
if [ -f "$out/ca.crt" ]; then
	exit 0
fi

openssl req -x509 -newkey rsa:2048 -nodes \
	-keyout "$out/ca.key" -out "$out/ca.crt" \
	-days 3650 -subj "/CN=dbclient test CA" >/dev/null 2>&1

openssl req -newkey rsa:2048 -nodes \
	-keyout "$out/server.key" -out "$out/server.csr" \
	-subj "/CN=pg.internal" >/dev/null 2>&1

printf 'subjectAltName=DNS:pg.internal\nbasicConstraints=CA:FALSE\n' >"$out/server.ext"
openssl x509 -req -in "$out/server.csr" \
	-CA "$out/ca.crt" -CAkey "$out/ca.key" -CAcreateserial \
	-out "$out/server.crt" -days 3650 -extfile "$out/server.ext" >/dev/null 2>&1

chmod 600 "$out/server.key"

# The `local` line is for `pg_isready`, which reaches the server over the Unix
# socket from inside the container and would otherwise be refused by the same
# rule that keeps the tests honest.
{
	echo 'local all all trust'
	echo 'hostssl all all all scram-sha-256'
} >"$out/pg_hba.conf"
