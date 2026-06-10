# Jeryu Split Repo Standard

Source commit: `cbecf7caa0e932c76a341b2521e66e911233860d`
Split repo: `jeryu-deploy`
Required check: `jeryu-deploy/required`

Required local commands are `just fast`, `just check`, `just score`, and
`just security`. Release-supporting repos also expose `just artifact-support`.

Bootstrap score files under `.jankurai/` are placeholders until the pinned
Jankurai lane runs. Do not treat them as green audit evidence.
