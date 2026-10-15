# Native commit status responses retain history. Compare UTC timestamps with
# nanosecond padding, independent of response order; conflicting ties fail closed.
def status_time:
  (.updated_at // .created_at) as $t
  | if ($t | type) != "string" then error("missing status timestamp")
    elif ($t | test("^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\\.[0-9]{1,9})?Z$"))
    then ($t | rtrimstr("Z") | split(".") | .[0] + "." + (((.[1] // "") + "000000000")[0:9]))
    else error("invalid UTC status timestamp") end;
def latest_statuses:
  .statuses
  | if type != "array" then error("missing status history") else . end
  | map(if (.context | type) != "string" or (.state | type) != "string"
        then error("invalid status entry") else . + {_time: status_time} end)
  | group_by(.context)
  | map((map(._time) | max) as $latest | map(select(._time == $latest))
        | if (map(.state) | unique | length) != 1 then error("conflicting latest statuses")
          else .[0] | del(._time) end);
