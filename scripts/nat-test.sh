#!/usr/bin/env bash
# Runs bots through real NATs, made by the Linux kernel in network namespaces.
#
#   scripts/nat-test.sh [cone|symmetric]        (default: cone)
#
#   home1 (192.168.1.2) -- router1 (NAT) --+
#                                          +-- internet -- open (10.0.0.2)
#   home2 (192.168.2.2) -- router2 (NAT) --+
#
# The bot in `open` creates a meeting. The bots in the two homes join it by its ticket.
# A meeting of three is wired each to each, so the two homes must connect to each other
# through both routers: that is the hole punch.
#
#   cone       the routers keep a node's port whatever it sends to (most home routers).
#              Expected: every bot has two links, and a bot behind a router finds from
#              two STUN servers that it is "behind a router".
#   symmetric  the routers pick a new port per destination. Punching cannot work.
#              Expected: the homes have one link each, and still hear each other
#              through the open node; a bot behind a router finds that its network
#              "gives a port per destination".
#
# Needs Linux with ip, nft and unshare, and either root or unprivileged user namespaces.
# Build first: cargo build -p palaver-bot

set -euo pipefail

kind="${1:-cone}"
bot="${PALAVER_BOT:-$(dirname "$0")/../target/debug/palaver-bot}"
seconds="${PALAVER_NAT_SECONDS:-8}"

if [ -z "${PALAVER_NAT_INSIDE:-}" ]; then
    [ -x "$bot" ] || { echo "no bot at $bot: cargo build -p palaver-bot" >&2; exit 2; }
    # A private user, mount and network namespace: nothing outside is touched, and
    # everything made here goes when this process does.
    export PALAVER_NAT_INSIDE=1
    if [ "$(id -u)" = 0 ]; then
        exec unshare --mount --net "$0" "$@"
    fi
    exec unshare --map-root-user --mount --net "$0" "$@"
fi

case "$kind" in
    cone) masquerade="masquerade" ;;
    symmetric) masquerade="masquerade random" ;;
    *) echo "unknown kind: $kind" >&2; exit 2 ;;
esac

out="$(mktemp -d)"
trap 'kill $(jobs -p) 2>/dev/null || true; rm -rf "$out"' EXIT

mount -t tmpfs tmpfs /run
mkdir -p /run/netns
for ns in internet open router1 router2 home1 home2; do
    ip netns add "$ns"
    ip -n "$ns" link set lo up
done

# link <ns a> <address a> <ns b> <address b>: a cable between two namespaces.
n=0
link() {
    n=$((n + 1))
    ip link add "v${n}a" netns "$1" type veth peer name "v${n}b" netns "$3"
    ip -n "$1" addr add "$2" dev "v${n}a"
    ip -n "$3" addr add "$4" dev "v${n}b"
    ip -n "$1" link set "v${n}a" up
    ip -n "$3" link set "v${n}b" up
}

link internet 10.0.0.1/24 open 10.0.0.2/24
link internet 10.0.1.1/24 router1 10.0.1.2/24      # router1's outside is v2b
link internet 10.0.2.1/24 router2 10.0.2.2/24      # router2's outside is v3b
link router1 192.168.1.1/24 home1 192.168.1.2/24
link router2 192.168.2.1/24 home2 192.168.2.2/24

ip -n open route add default via 10.0.0.1
ip -n router1 route add default via 10.0.1.1
ip -n router2 route add default via 10.0.2.1
ip -n home1 route add default via 192.168.1.1
ip -n home2 route add default via 192.168.2.1
ip netns exec internet sysctl -qw net.ipv4.ip_forward=1

# A router: forwards, rewrites the source of what leaves by its outside, and lets in
# from outside only answers to what went out. What is sent to the router itself from
# outside is dropped without a word, as home routers do.
router() {
    ip netns exec "$1" sysctl -qw net.ipv4.ip_forward=1
    ip netns exec "$1" nft -f - <<RULES
table ip nat {
    chain post { type nat hook postrouting priority 100; oifname "$2" $masquerade; }
}
table ip filter {
    chain forward {
        type filter hook forward priority 0; policy drop;
        ct state established,related accept
        iifname != "$2" accept
    }
    chain input {
        type filter hook input priority 0; policy accept;
        iifname "$2" ct state new drop
    }
}
RULES
}
router router1 v2b
router router2 v3b

# Two STUN servers at different addresses, as seen from the homes.
ip netns exec open "$bot" stun-server 10.0.0.2:3478 &
ip netns exec internet "$bot" stun-server 10.0.1.1:3478 &
stun="--stun 10.0.0.2:3478 --stun 10.0.1.1:3478"

# What a node behind a router learns of its network, before any meeting.
# shellcheck disable=SC2086
ip netns exec home1 "$bot" create $stun --silent --seconds 0 >"$out/network" 2>&1

# shellcheck disable=SC2086
ip netns exec open "$bot" create --bind 10.0.0.2:4433 $stun --silent --speaker 0 \
    --seconds "$seconds" >"$out/open" 2>&1 &
bots="$!"
for _ in $(seq 50); do
    grep -q '^ticket ' "$out/open" && break
    sleep 0.1
done
ticket="$(sed -n 's/^ticket //p' "$out/open")"
[ -n "$ticket" ] || { echo "the open bot gave no ticket" >&2; cat "$out/open" >&2; exit 1; }

ip netns exec home1 "$bot" join "$ticket" --speaker 1 --seconds "$seconds" >"$out/home1" 2>&1 &
bots="$bots $!"
ip netns exec home2 "$bot" join "$ticket" --speaker 2 --seconds "$seconds" >"$out/home2" 2>&1 &
bots="$bots $!"
# The STUN servers run until the script ends; only the bots are waited for.
# shellcheck disable=SC2086
wait $bots
if [ -n "${PALAVER_NAT_VERBOSE:-}" ]; then tail -n +1 "$out"/home1 "$out"/home2; fi

# What each bot said two seconds before the end: late enough that the meeting has
# settled, and before any of them has begun to leave.
last() { grep "^second $((seconds - 2)):" "$out/$1" || true; }
for who in open home1 home2; do
    echo "$who: $(last "$who")"
done

fail=0
expect() {
    case "$(last "$1")" in
        *"$2"*) ;;
        *) echo "FAIL: $1 should say '$2'" >&2; fail=1 ;;
    esac
}
echo "home1 alone: $(grep '^network ' "$out/network")"
echo "open: $(grep '^network ' "$out/open")"
network() {
    grep -q "^network $2\$" "$out/$1" || { echo "FAIL: $1 should say 'network $2'" >&2; fail=1; }
}
network open "open"
if [ "$kind" = cone ]; then
    network network "behind a router"
else
    network network "gives a port per destination"
fi
expect open "hears [1, 2]"
expect home1 "hears [2]"
expect home2 "hears [1]"
if [ "$kind" = cone ]; then
    expect home1 "links 2"
    expect home2 "links 2"
else
    expect home1 "links 1"
    expect home2 "links 1"
fi
[ "$fail" = 0 ] && echo "ok: $kind"
exit "$fail"
