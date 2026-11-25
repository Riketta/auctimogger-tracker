-- Parses the lua-language-server `--check` report and exits non-zero when the
-- codebase is not type-clean. Historically the LLS CLI exit code is unreliable,
-- so we decide from the report itself and fail safe when it is unparseable.

local path = assert(arg and arg[1], "usage: lua scripts/assert_lls_clean.lua <lls.log>")

local file = assert(io.open(path, "r"), "cannot open report: " .. path)
local content = file:read("*a")
file:close()

local problems = tonumber(content:match("found%s+(%d+)%s+problems?"))
    or tonumber(content:match("(%d+)%s+problems?%s+found"))
    or (content:match("no problems found") and 0)
    or nil
if not problems then
    io.stderr:write("could not parse lua-language-server report; failing safe:\n\n" .. content .. "\n")
    os.exit(1)
end

if problems > 0 then
    io.stderr:write(content .. "\n")
    os.exit(1)
end

print("lls clean: 0 problems")
