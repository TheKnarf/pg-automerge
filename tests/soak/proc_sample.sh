# shellcheck shell=sh
# Run inside the soak's server container (docker exec -i .. sh -s): one CSV
# line per postgres process: pid,rss_kb,anon_kb,file_kb,shmem_kb,"title"
# (the process title, e.g. "postgres: postgres app 172.17.0.1(4242) idle"),
# then one line "cgroup,<memory.current>,<anon>,<file>,<shmem>" (bytes).
for d in /proc/[0-9]*; do
    title=$(tr '\0' ' ' <"$d/cmdline" 2>/dev/null) || continue
    case "$title" in postgres*) ;; *) continue ;; esac
    awk -v pid="${d#/proc/}" -v title="$title" '
        $1 == "VmRSS:" { rss = $2 } $1 == "RssAnon:" { anon = $2 }
        $1 == "RssFile:" { file = $2 } $1 == "RssShmem:" { shmem = $2 }
        END { gsub(/"/, "", title); sub(/ +$/, "", title); printf "%s,%s,%s,%s,%s,\"%s\"\n", pid, rss, anon, file, shmem, title }
    ' "$d/status" 2>/dev/null
done
awk '$1 == "anon" { a = $2 } $1 == "file" { f = $2 } $1 == "shmem" { s = $2 } END { printf "cgroup,%s,%s,%s,%s\n", cur, a, f, s }' \
    cur="$(cat /sys/fs/cgroup/memory.current)" /sys/fs/cgroup/memory.stat
