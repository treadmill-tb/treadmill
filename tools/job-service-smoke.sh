#!/usr/bin/env bash
# Smoke-test a job's gateway services against a running `nix run .#devstack`.
#
#   tools/job-service-smoke.sh <manifest-digest> [<ghcr-repository>]
#
# Registers an image built by the images workflow, enqueues a job on it, waits
# for the puppet to announce a service, mints a token for it, and drives the
# gateway with that token, without one, and at another job's host. Set
# TML_TEST_JOB_ID to reuse a job that is already running instead of enqueueing.
#
# Needs `jq`, which the default dev shell does not carry:
#   nix shell nixpkgs#jq --command tools/job-service-smoke.sh <digest>
set -euo pipefail

digest="${1:?usage: tools/job-service-smoke.sh <manifest-digest> [<repository>]}"
repo="${2:-treadmill-tb/ubuntu-server-2604}"
registry="${TML_TEST_REGISTRY:-ghcr.io}"

sb="http://127.0.0.1:${TML_SB_PORT:-8000}"
gw_domain="${TML_GW_DOMAIN:-jobgw.localhost}"
gw_port="${TML_GW_PORT:-8443}"
job_service_hostfwd_port=3860
service=webterm

# The devstack's seeded API token (tools/devstack.sh).
api_token="B1oy2ko1wVdGKbvKc/9dKi7ggZYLTLzdm2As4CWV15c="
auth=(-H "Authorization: Bearer $api_token" -H 'content-type: application/json')

say() { printf '\n=== %s\n' "$*"; }

# A JWT payload is base64url with the padding stripped; put it back before decoding.
decode_jwt() {
	local payload
	payload="$(cut -d. -f2 <<<"$1" | tr '_-' '/+')"
	while [ $((${#payload} % 4)) -ne 0 ]; do payload="$payload="; done
	base64 -d <<<"$payload" | jq .
}

say "Registering $registry/$repo@$digest"
if curl -fsS -X POST "$sb/api/v1/images/$digest/sources" "${auth[@]}" \
	-d "{\"registry\":\"$registry\",\"repository\":\"$repo\"}" >/dev/null 2>&1; then
	echo "registered"
else
	echo "already registered (or registration refused); continuing"
fi

# Re-runnable: reuse the set from an earlier run rather than failing on the
# duplicate name.
say "Public image set for it"
set_name=ubuntu-webterm
set_id="$(curl -fsS -X POST "$sb/api/v1/image-sets" "${auth[@]}" \
	-d "{\"name\":\"$set_name\",\"label\":\"Ubuntu 26.04 (webterm)\",\"public\":true}" 2>/dev/null |
	jq -r .id 2>/dev/null || true)"
if [ -z "$set_id" ] || [ "$set_id" = null ]; then
	set_id="$(curl -fsS "$sb/api/v1/image-sets" "${auth[@]}" |
		jq -r --arg n "$set_name" '.[] | select(.name == $n) | .id' | head -n1)"
	echo "reusing set $set_id"
else
	echo "created set $set_id"
fi
[ -n "$set_id" ] || { echo "!! no image set to use" >&2; exit 1; }
curl -fsS -o /dev/null -X POST "$sb/api/v1/image-sets/$set_id/generations" "${auth[@]}" \
	-d "{\"members\":[{\"manifest_digest\":\"$digest\",\"required_host_tags\":[]}]}"

if [ -n "${TML_TEST_JOB_ID:-}" ]; then
	say "Using the already-running job $TML_TEST_JOB_ID"
	job_id="$TML_TEST_JOB_ID"
else
	say "Enqueueing a job"
	job_id="$(curl -fsS -X POST "$sb/api/v1/jobs" "${auth[@]}" \
		-d "{\"init_spec\":{\"type\":\"image_set\",\"set_id\":\"$set_id\"},
		     \"restart_policy\":{\"max_restarts\":0},
		     \"parameters\":{},
		     \"override_timeout\":null,
		     \"label\":\"webterm smoke\"}" | jq -r '.job_id')"
	echo "job $job_id"
fi

say "Waiting for the job to report an address and announce $service"
for _ in $(seq 1 450); do
	info="$(curl -fsS "$sb/api/v1/jobs/$job_id" "${auth[@]}")"
	state="$(jq -r '.state' <<<"$info")"
	addr="$(jq -r '.job_ip_address // empty' <<<"$info")"
	names="$(jq -r '[.services[].name] | join(",")' <<<"$info")"
	printf '\r  state=%-14s addr=%-15s services=[%s]   ' "$state" "${addr:--}" "$names"
	if [ -n "$addr" ] && jq -e --arg s "$service" 'any(.services[]; .name == $s)' >/dev/null <<<"$info"; then
		echo
		break
	fi
	sleep 2
done
jq '{state, job_ip_address, services}' <<<"$info"
jq -e --arg s "$service" 'any(.services[]; .name == $s)' >/dev/null <<<"$info" ||
	{ echo "!! $service was never announced" >&2; exit 1; }

say "Minting a token for $service"
creds="$(curl -fsS -X POST "$sb/api/v1/jobs/$job_id/services/$service/token" "${auth[@]}")"
token="$(jq -r .token <<<"$creds")"
jq '{url, domains, expires_at}' <<<"$creds"
echo "  claims:"
decode_jwt "$token" | sed 's/^/    /'

host="$service-$job_id.$gw_domain"
gw() { curl -sS -k --resolve "$host:$gw_port:127.0.0.1" -o /dev/null -w '%{http_code}' "$@"; }

# The job's own proxy, reached through the QEMU hostfwd with the gateway
# bypassed: it validates the same token, so a sibling job that reaches the port
# directly still gets nothing without one.
direct() {
	curl -sS -H "Host: $host" -o /dev/null -w '%{http_code}' \
		"http://127.0.0.1:$job_service_hostfwd_port/" "$@"
}

say "The job's own proxy, gateway bypassed"
echo "  without a token  : HTTP $(direct)   (expect 401)"
echo "  with the token   : HTTP $(direct -H "X-Tml-Token: $token")   (expect 200)"
echo "  another service  : HTTP $(curl -sS -H "Host: other-$job_id.$gw_domain" \
	-H "X-Tml-Token: $token" -o /dev/null -w '%{http_code}' \
	"http://127.0.0.1:$job_service_hostfwd_port/")   (expect 404)"

say "Through the gateway at https://$host:$gw_port/"
echo "  with the token   : HTTP $(gw "https://$host:$gw_port/?tml_token=$token")   (expect 302, promoting it to a cookie)"
echo "  without a token  : HTTP $(gw "https://$host:$gw_port/")   (expect 401)"
echo "  in a header      : HTTP $(gw -H "X-Tml-Token: $token" "https://$host:$gw_port/")   (expect 200)"
wrong="other-$job_id.$gw_domain"
echo "  at another host  : HTTP $(curl -sS -k --resolve "$wrong:$gw_port:127.0.0.1" \
	-o /dev/null -w '%{http_code}' "https://$wrong:$gw_port/?tml_token=$token")   (expect 403)"

cat <<EOF

Open in a browser (self-signed, so accept the warning once):

  https://$host:$gw_port/?tml_token=$token

Job:  $sb/api/v1/jobs/$job_id
Stop: curl -fsS -X DELETE $sb/api/v1/jobs/$job_id -H "Authorization: Bearer \$api_token"
EOF
