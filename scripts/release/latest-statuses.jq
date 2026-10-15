# Native commit status responses retain history. Compare UTC timestamps with
# nanosecond padding, independent of response order; conflicting ties fail closed.
def status_time:
  (.updated_at // .created_at) as $t
  | "^(?<second>[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2})(?:\\.(?<fraction>[0-9]{1,9}))?(?<zone>Z|[+-][0-9]{2}:[0-9]{2})$" as $pattern
  | if ($t | type) != "string" then error("missing status timestamp")
    elif ($t | test($pattern) | not) then error("invalid RFC3339 status timestamp")
    else ($t | capture($pattern)) as $p
      | (if $p.zone == "Z" then 0
         else ($p.zone[1:3] | tonumber) as $h | ($p.zone[4:6] | tonumber) as $m
           | if $h > 23 or $m > 59 then error("invalid timestamp offset")
             else ($h * 3600 + $m * 60) * (if $p.zone[0:1] == "+" then 1 else -1 end) end
         end) as $offset
      | [(($p.second + "Z" | fromdateiso8601) - $offset),
         ((($p.fraction // "") + "000000000")[0:9] | tonumber)]
    end;
def latest_statuses:
  .statuses
  | if type != "array" then error("missing status history") else . end
  | map(if (.context | type) != "string" or (.state | type) != "string"
        then error("invalid status entry") else . + {_time: status_time} end)
  | group_by(.context)
  | map((map(._time) | max) as $latest | map(select(._time == $latest))
        | if (map(.state) | unique | length) != 1 then error("conflicting latest statuses")
          else .[0] | del(._time) end);
