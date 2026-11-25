-- CI guard, two directions:
--   1. every Lua file in the addon folder must be listed in the TOC (busted/
--      LLS load modules via loadfile/require and would never notice a missing
--      TOC entry, but the game only loads what the TOC lists);
--   2. every file listed in the TOC must exist on disk (the game errors on a
--      missing load entry; require-based tooling would never notice).
--
-- Usage: lua scripts/check_toc.lua <path/to/Addon.toc>
--
-- Works from any current directory: everything resolves from the TOC path.
-- The folder walk uses luafilesystem (lfs) when available and falls back to
-- `find` through io.popen. Without either (cmd.exe ships a different `find`)
-- the walk yields nothing; that is reported as a failure, never as a pass.
-- Exit code: 0 = in sync, 1 = problems, 2 = usage error.

local tocArg = arg and arg[1]
if not tocArg then
    io.stderr:write("usage: lua scripts/check_toc.lua <path/to/Addon.toc>\n")
    os.exit(2)
end

--- Reports a broken setup and exits non-zero.
---@param message string
local function fail(message)
    io.stderr:write("check_toc: " .. message .. "\n")
    os.exit(1)
end

--- Reads a whole file in binary mode (no line-ending translation), or fails
--- with `message`.
---@param path string
---@param message string
---@return string text
local function readFile(path, message)
    local file = assert(io.open(path, "rb"), message)
    local text = file:read("*a")
    file:close()
    return text
end

local ok, lfs = pcall(require, "lfs")
local hasLfs = ok and type(lfs) == "table" and type(lfs.dir) == "function"

--- Quotes a path for a POSIX shell (the io.popen fallback).
---@param path string
---@return string
local function shellQuote(path)
    return "'" .. path:gsub("'", "'\\''") .. "'"
end

--- Lists every .lua file below dir, as slash-separated paths relative to it.
---@param dir string
---@return string[] paths sorted
local function walk(dir)
    local found = {}
    if hasLfs then
        local function visit(path, prefix)
            for name in lfs.dir(path) do
                if name ~= "." and name ~= ".." then
                    local child = path .. "/" .. name
                    local mode = lfs.attributes(child, "mode")
                    if mode == "directory" then
                        visit(child, prefix .. name .. "/")
                    elseif mode == "file" and name:match("%.lua$") then
                        found[#found + 1] = prefix .. name
                    end
                end
            end
        end
        visit(dir, "")
    else
        local pipe = assert(io.popen("find " .. shellQuote(dir) .. " -type f -name '*.lua'"), "io.popen is unavailable")
        for line in pipe:lines() do
            -- `find .` prefixes entries with ./; TOC entries are folder-relative.
            local rel = line:gsub("\\", "/"):sub(#dir + 1):gsub("^/", "")
            if rel:match("%.lua$") then
                found[#found + 1] = rel
            end
        end
        pipe:close()
    end
    table.sort(found)
    return found
end

-- The TOC is relative to its own folder.
local tocPath = tocArg:gsub("\\", "/"):gsub("/+$", "")
local addonDir = tocPath:match("^(.*)/[^/]+$") or "."
local text = readFile(tocPath, "cannot open TOC: " .. tocArg)
text = text:gsub("^\239\187\191", "") -- a UTF-8 BOM on an otherwise fine TOC

---@type string[]
local entries = {}
local listed = {}
for line in (text .. "\n"):gmatch("([^\r\n]*)\r?\n") do
    local entry = line:match("^%s*(.-)%s*$"):gsub("\\", "/")
    if entry ~= "" and entry:sub(1, 1) ~= "#" then
        if not listed[entry] then
            listed[entry] = true
            entries[#entries + 1] = entry
        end
    end
end

local found = walk(addonDir)
if #found == 0 then
    fail("no lua file found under " .. addonDir .. " (the walk needs luafilesystem or a Unix find); refusing to pass")
end

---@type string[]
local missing = {} -- on disk, not in the TOC
for _, path in ipairs(found) do
    if not listed[path] then
        missing[#missing + 1] = path
    end
end

---@type string[]
local absent = {} -- in the TOC, not on disk
for _, entry in ipairs(entries) do
    local handle = io.open(addonDir .. "/" .. entry, "rb")
    if handle then
        handle:close()
    else
        absent[#absent + 1] = entry
    end
end

if #missing > 0 or #absent > 0 then
    io.stderr:write("toc coverage problems for " .. tocArg .. ":\n")
    if #missing > 0 then
        io.stderr:write("  lua files present under " .. addonDir .. " but missing from the TOC:\n")
        for _, path in ipairs(missing) do
            io.stderr:write("    " .. path .. "\n")
        end
    end
    if #absent > 0 then
        io.stderr:write("  files listed in the TOC but missing on disk:\n")
        for _, path in ipairs(absent) do
            io.stderr:write("    " .. path .. "\n")
        end
    end
    os.exit(1)
end

print(
    ("toc coverage ok: %s: %d lua file(s) under %s, all listed; %d TOC entries, all present"):format(
        tocArg,
        #found,
        addonDir,
        #entries
    )
)
