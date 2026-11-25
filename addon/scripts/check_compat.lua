-- CI guard: keeps the compat manifest from drifting.
--
-- Adapters/compat.lua is the single source of truth for every game surface
-- the addon touches (addon/SPEC.md section 7.4). This script loads that
-- manifest and diffs it against the lint allowlists and against the source:
--
--   (a) the manifest is read straight out of Adapters/compat.lua, run as the
--       client runs it (with a fake `...`);
--   (b) addon/.luacheckrc is loaded in a sandbox (it may build its lists
--       with the pure part of the standard library, nothing else) and
--   (c) diagnostics.globals is read out of addon/.luarc.json;
--   (d) the three must agree: every manifest global (required + optional +
--       authored) and the public global GetAuctionBuyout are allowed for the
--       adapters in both lint files, every name either lint file allows is
--       in the manifest or in the TOOLING set below, and the Core rule of
--       .luacheckrc allows no global at all;
--   (e) every Lua file the TOC loads (the manifest itself excepted) is
--       scanned: an event-name literal no manifest tier lists, the token
--       C_AuctionHouse anywhere in code or in a string, any other C_*
--       namespace the manifest does not list, and - for Core/ - any bare
--       manifest global name (or _G) used as an identifier.
--
-- Known limitations (accepted, conservative by design):
--   * the source scan is a token walk over comment-stripped text, not a
--     parse: a Core field that merely SHARES a manifest name (`record.time`)
--     is flagged too - rename it, Core owns no game name. A Core local does
--     pass: the settings module is `local Settings`, and `Settings` is also
--     a game global, so a name the file itself declares with `local`, used
--     at or after that declaration, is read as the local it is;
--   * an adapter global outside the C_ family is policed by luacheck (the
--     allowlist IS the manifest), not by this scan;
--   * a global reached through an alias (`local create = CreateFrame`) is
--     still seen where it is aliased; C_ namespaces are matched by
--     namespace, so every member of a listed namespace passes;
--   * reverse drift (a manifest name nothing uses) is never flagged.
--
-- Usage: lua scripts/check_compat.lua [<addon .toc path>]
-- Exit code: 0 = in sync; 1 = drift or broken setup.

local scriptSource = debug.getinfo(1, "S").source or ""
local scriptPath = scriptSource:match("^@(.*)$") or (arg and arg[0]) or ""
local scriptDir = scriptPath:match("^(.*)[/\\][^/\\]+$") or "."
local addonRoot = scriptDir .. "/.."

local ADDON = "AuctiMoggerDatabase" -- the namespace, and the folder that carries it
local DEFAULT_TOC = ADDON .. "/" .. ADDON .. ".toc"
local MANIFEST_ENTRY = "Adapters/compat.lua" -- relative to the addon folder
local CORE_PREFIX = "Core/" -- the TOC entries below it are the pure modules
local LUACHECKRC_PATH = addonRoot .. "/.luacheckrc"
local LUARC_PATH = addonRoot .. "/.luarc.json"
local RC = "addon/.luacheckrc" -- how the lint config is named in messages
local LUARC = "addon/.luarc.json"
local ADAPTERS_RULE = ADDON .. "/Adapters" -- files[...] keys, as luacheck sees them
local CORE_RULE = ADDON .. "/Core"
local PUBLIC_GLOBAL = "GetAuctionBuyout" -- the global the addon defines through rawset
local FORBIDDEN_NAMESPACE = "C_AuctionHouse"
local PURE_GLOBAL = "_G" -- Core must not even reach for the global table

local GLOBAL_TIERS = { "requiredGlobals", "optionalGlobals", "authored" }
local EVENT_TIERS = { "requiredEvents", "optionalEvents" }
local ALLOW_FIELDS = { "globals", "read_globals", "new_globals" }

-- Names the lint allowlists may carry WITHOUT a manifest entry: the busted
-- harness (spec files) and `arg`, the standalone interpreter's script
-- arguments. Nothing game-facing may ever be added here.
local TOOLING = {
    after_each = true,
    arg = true,
    before_each = true,
    describe = true,
    expose = true,
    insulate = true,
    it = true,
    lazy_setup = true,
    lazy_teardown = true,
    match = true,
    mock = true,
    pending = true,
    setup = true,
    spy = true,
    stub = true,
    teardown = true,
}

-- Messages are collected first so one run reports every problem.
---@type string[]
local problems = {}

--- Records one problem with its location: `where:line:` for a source
--- finding, `where:` for a finding about a lint file.
---@param where string file the problem belongs to
---@param line number|nil 1-based source line, when known
---@param message string
local function addProblem(where, line, message)
    if line then
        problems[#problems + 1] = ("%s:%d: %s"):format(where, line, message)
    else
        problems[#problems + 1] = where .. ": " .. message
    end
end

--- Reports a setup the guard cannot work without and exits non-zero.
---@param message string
local function fail(message)
    io.stderr:write("check_compat: " .. message .. "\n")
    os.exit(1)
end

--- A shallow copy of a standard library table, so that a sandboxed config
--- cannot mutate the real one.
---@param lib table
---@return table copy
local function copyLibrary(lib)
    local copy = {}
    for name, value in pairs(lib) do
        copy[name] = value
    end
    return copy
end

-- What addon/.luacheckrc may read while the guard runs it: the pure part of
-- the standard library. A config that needs anything else is a broken setup.
local SAFE_STDLIB = {
    assert = assert,
    error = error,
    ipairs = ipairs,
    math = copyLibrary(math),
    next = next,
    pairs = pairs,
    pcall = pcall,
    rawequal = rawequal,
    rawget = rawget,
    rawset = rawset,
    select = select,
    setmetatable = setmetatable,
    string = copyLibrary(string),
    table = copyLibrary(table),
    tonumber = tonumber,
    tostring = tostring,
    type = type,
    unpack = unpack,
}

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

--- Loads a Lua file so that it cannot touch this script's globals: whatever
--- the chunk writes lands in env, `_G` included. Text chunks only.
---@param path string
---@param env table the chunk's environment
---@return function chunk
local function loadSandboxed(path, env)
    env._G = env
    if setfenv then
        local chunk = assert(loadfile(path), "cannot load " .. path)
        setfenv(chunk, env)
        return chunk
    end
    -- Lua 5.2+ binds the environment at load time and only there does a mode
    -- argument exist (this file is type-checked against the Lua 5.1 API).
    ---@diagnostic disable-next-line: redundant-parameter
    return assert(loadfile(path, "t", env), "cannot load " .. path)
end

--- Sorts the keys of a set so reports do not depend on hash order.
---@param set table<string, boolean>
---@return string[]
local function sortedKeys(set)
    local keys = {}
    for key in pairs(set) do
        keys[#keys + 1] = key
    end
    table.sort(keys)
    return keys
end

--- Lists the file entries of a TOC, relative to the TOC's own folder.
---@param path string
---@return string[] entries slash-separated, in file order
---@return string dir the TOC's folder
local function readToc(path)
    local text = readFile(path, "cannot open TOC: " .. path)
    local dir = path:gsub("\\", "/"):match("^(.*)/[^/]+$") or "."
    local entries = {}
    for line in (text .. "\n"):gmatch("([^\r\n]*)\r?\n") do
        local entry = line:match("^%s*(.-)%s*$"):gsub("\\", "/")
        if entry ~= "" and entry:sub(1, 1) ~= "#" then
            entries[#entries + 1] = entry
        end
    end
    return entries, dir
end

--- (a) Runs Adapters/compat.lua the way the client does - the namespace
--- name and a fresh namespace table as `...` - and returns the manifest it
--- publishes: A.Compat.manifest, or A.Compat itself when `.manifest` is
--- absent.
---@param addonDir string
---@return table manifest
local function readManifest(addonDir)
    local path = addonDir .. "/" .. MANIFEST_ENTRY
    local namespace = {}
    -- Reads fall through to this script's globals, writes stay in the sandbox.
    local env = setmetatable({}, { __index = _G })
    local chunk = loadSandboxed(path, env)
    local ok, result = pcall(chunk, ADDON, namespace)
    if not ok then
        fail("cannot run " .. MANIFEST_ENTRY .. ": " .. tostring(result))
    end
    -- Built without gaps: ipairs stops at a nil, and a module that only
    -- returns its table leaves the namespace empty.
    local candidates = {}
    ---@param value any
    local function addCandidate(value)
        if type(value) == "table" then
            candidates[#candidates + 1] = value
        end
    end
    addCandidate(namespace.Compat)
    if type(result) == "table" then
        addCandidate(result.Compat)
        if result.Compat ~= result then
            addCandidate(result)
        end
    end
    for _, candidate in ipairs(candidates) do
        if type(candidate) == "table" then
            if type(candidate.manifest) == "table" then
                return candidate.manifest
            end
            if type(candidate.requiredGlobals) == "table" then
                return candidate
            end
        end
    end
    fail("cannot find the manifest in " .. MANIFEST_ENTRY .. " (expected A.Compat.manifest)")
    return {}
end

--- Indexes one manifest tier, refusing a manifest that does not follow the
--- documented shape instead of silently checking against nothing.
---@param manifest table
---@param tier string
---@return table<string, boolean> set
local function tierSet(manifest, tier)
    local list = manifest[tier]
    if type(list) ~= "table" then
        fail("manifest tier " .. tier .. " is missing from " .. MANIFEST_ENTRY)
    end
    local set = {}
    for _, name in ipairs(list) do
        if type(name) ~= "string" or name == "" then
            fail("manifest tier " .. tier .. " holds an entry that is not a name")
        end
        set[name] = true
    end
    return set
end

--- (c) Reads diagnostics.globals out of .luarc.json. It is an array of
--- plain names, so a pattern is enough (plain Lua 5.1 has no JSON reader).
---@param path string
---@return table<string, boolean> set
local function readLuarcGlobals(path)
    local text = readFile(path, "cannot open " .. LUARC)
    local body = text:match("\"diagnostics%.globals\"%s*:%s*(%b[])")
    if not body then
        fail(LUARC .. " has no diagnostics.globals array")
    end
    local set = {}
    for name in body:gmatch("\"([^\"\\]+)\"") do
        set[name] = true
    end
    if next(set) == nil then
        fail(LUARC .. ": diagnostics.globals is empty")
    end
    return set
end

--- (b) Collects the globals an options table - the top level of .luacheckrc
--- or one of its files[...] rules - allows, across the three list fields.
---@param options table|nil
---@param where string location used in messages
---@return table<string, boolean> set
local function allowedGlobals(options, where)
    local set = {}
    for _, field in ipairs(ALLOW_FIELDS) do
        local list = options and options[field]
        if list ~= nil then
            if type(list) ~= "table" then
                addProblem(where, nil, field .. " must be a table")
            else
                for _, name in ipairs(list) do
                    if type(name) == "string" then
                        set[name] = true
                    else
                        addProblem(where, nil, field .. " holds an entry that is not a name")
                    end
                end
            end
        end
    end
    return set
end

-- (e) the source scan ---------------------------------------------------------

local NEWLINE = 10
local DQUOTE = 34
local SQUOTE = 39
local MINUS = 45
local BACKSLASH = 92
local LBRACKET = 91
local NAMESPACE_PATTERN = "%f[%w_](C_[%a][%w_]*)"

-- Curated ALL_CAPS string literals that are NOT events, each with its
-- reason; extend only when the scan flags a real constant.
local NON_EVENT_LITERALS = {
    ANCHOR_NONE = "the tooltip's anchor type (addon/SPEC.md section 7.4)",
    ANCHOR_CURSOR = "the GameTooltip owner anchor of the options panel's hover help (section 7.8)",
    SLASH_AMDB1 = "the slash command's global; set through rawset by name (section 7.7)",
    SLASH_AMDB2 = "the slash command's alias global; set through rawset by name (section 7.7)",
}

--- Walks Lua source once: comments and string literals are replaced by
--- whitespace (line structure preserved) and every short-string literal is
--- recorded with its start line. Short strings (with backslash escapes),
--- long strings, long comments and line comments are all handled, so `--`
--- inside a string never swallows code and commented-out code is never
--- scanned. Long-string literals are blanked but not recorded (event names
--- never live in them).
---
--- Untouched source is copied in runs instead of byte by byte: a generated
--- market file is tens of megabytes, and a table slot per byte would not fit.
---@param text string
---@return string code source with comments and strings blanked (identifiers remain)
---@return {value: string, line: number}[] literals every short-string literal, in source order
local function stripCommentsAndCollect(text)
    local out = {}
    local literals = {}
    local n = #text
    local line = 1
    local plain = 1 -- first byte of the untouched run not copied yet

    --- Copies the untouched run that ends right before `from`.
    ---@param from number
    local function flushCode(from)
        if plain < from then
            out[#out + 1] = text:sub(plain, from - 1)
        end
    end

    --- Replaces text[from..to] (a comment or a string literal) with whitespace:
    --- one byte per byte, newlines kept, so byte offsets stay valid.
    ---@param from number
    ---@param to number
    local function blank(from, to)
        flushCode(from)
        local chunk = text:sub(from, to)
        local _, newlines = chunk:gsub("\n", "")
        if newlines > 0 then
            out[#out + 1] = ("\n"):rep(newlines)
        end
        if #chunk > newlines then
            out[#out + 1] = (" "):rep(#chunk - newlines)
        end
        line = line + newlines
        plain = to + 1
    end

    local i = 1
    while i <= n do
        local c = text:byte(i)
        if c == DQUOTE or c == SQUOTE then
            local startLine = line
            local value = {}
            local j = i + 1
            local runStart = j -- escapes cut the literal into runs
            while j <= n do
                local d = text:byte(j)
                if d == BACKSLASH then
                    value[#value + 1] = text:sub(runStart, j - 1)
                    value[#value + 1] = text:sub(j, j + 1)
                    j = j + 2
                    runStart = j
                elseif d == c then
                    break
                else
                    j = j + 1
                end
            end
            value[#value + 1] = text:sub(runStart, j - 1)
            literals[#literals + 1] = { value = table.concat(value), line = startLine }
            blank(i, math.min(j, n))
            i = j + 1
        elseif c == MINUS and text:byte(i + 1) == MINUS then
            local eqs = text:match("^%-%-%[(=*)%[", i)
            if eqs then
                local closer = "]" .. eqs .. "]"
                local closeAt = text:find(closer, i + 2, true)
                local stop = closeAt and (closeAt + #closer - 1) or n
                blank(i, stop)
                i = stop + 1
            else
                local nl = text:find("\n", i, true)
                blank(i, (nl or n + 1) - 1)
                i = nl or n + 1
            end
        elseif c == LBRACKET and text:match("^%[(=*)%[", i) then
            local eqs = text:match("^%[(=*)%[", i)
            local closer = "]" .. eqs .. "]"
            local closeAt = text:find(closer, i + 1, true)
            local stop = closeAt and (closeAt + #closer - 1) or n
            blank(i, stop)
            i = stop + 1
        else
            if c == NEWLINE then
                line = line + 1
            end
            i = i + 1
        end
    end
    flushCode(n + 1)
    return table.concat(out), literals
end

--- 1-based line of a byte offset in comment-stripped code. Stripping keeps
--- every newline at its original offset, so a count is enough - and it only
--- runs when a problem is reported, never per token.
---@param code string
---@param position number
---@return number
local function lineOf(code, position)
    local _, before = code:sub(1, position - 1):gsub("\n", "")
    return before + 1
end

--- Finds every `pattern` match in ascending order and calls `visit` with
--- the match start and the capture.
---@param text string
---@param pattern string with exactly one capture group
---@param visit fun(start: number, capture: string)
local function scanCaptures(text, pattern, visit)
    local pos = 1
    while true do
        local start, stop, capture = text:find(pattern, pos)
        if not start then
            break
        end
        visit(start, capture or "")
        pos = stop + 1
    end
end

--- An event name as the client registers it: upper case, digits and
--- underscores only, with at least one underscore (so "NONE" is not one).
---@param value string
---@return boolean
local function isEventLiteral(value)
    if NON_EVENT_LITERALS[value] or not value:find("_", 1, true) then
        return false
    end
    return value:match("^%u[%u%d_]*$") ~= nil
end

--- A plain name as a Lua pattern (names are identifiers; this is only a
--- guard against a manifest entry that is not).
---@param name string
---@return string
local function escapePattern(name)
    return (name:gsub("%W", "%%%1"))
end

--- Byte offset of the first place `code` declares `name` as a local (`local
--- name`, `local a, name`, `local function name`), or nil when it never does.
--- A token scan, not a parse: it is only used to tell a Core module's own
--- local apart from the game global of the same name.
---@param code string source with comments and strings blanked
---@param name string
---@return number|nil offset
local function firstLocalDeclaration(code, name)
    local token = "%f[%w_]" .. escapePattern(name) .. "%f[^%w_]"
    local first
    ---@param start number
    local function visit(start)
        if first == nil or start < first then
            first = start
        end
    end
    scanCaptures(code, "%f[%w_]local%s+" .. token, visit)
    scanCaptures(code, "%f[%w_]local%s+function%s+" .. token, visit)
    scanCaptures(code, "%f[%w_]local%s+[^=\n]-,%s*" .. token, visit)
    return first
end

-- the checks -------------------------------------------------------------------

local tocPath = (arg and arg[1]) or (addonRoot .. "/" .. DEFAULT_TOC)
local entries, addonDir = readToc(tocPath)
if #entries == 0 then
    fail("no file entries found in " .. tocPath)
end
local manifest = readManifest(addonDir)

---@type table<string, boolean>
local manifestGlobals = {}
for _, tier in ipairs(GLOBAL_TIERS) do
    for name in pairs(tierSet(manifest, tier)) do
        manifestGlobals[name] = true
    end
end
---@type table<string, boolean>
local manifestEvents = {}
for _, tier in ipairs(EVENT_TIERS) do
    for name in pairs(tierSet(manifest, tier)) do
        manifestEvents[name] = true
    end
end

-- (b) the lint config, in a sandbox whose reads are limited to SAFE_STDLIB
-- and whose writes never leave it
local rcEnv = setmetatable({ files = {} }, { __index = SAFE_STDLIB })
local rcChunk = loadSandboxed(LUACHECKRC_PATH, rcEnv)
local rcOk, rcRunErr = pcall(rcChunk)
if not rcOk then
    fail("cannot run " .. RC .. ": " .. tostring(rcRunErr))
end
local rules = rcEnv.files
if type(rules) ~= "table" then
    fail(RC .. " defines no files table")
end
if rcEnv.std ~= "lua51" then
    addProblem(RC, nil, "std must be 'lua51': the boundary rules are written against it")
end
local rcTop = allowedGlobals(rcEnv, RC)

--- A files[...] rule of the lint config; a missing rule is a broken setup,
--- not an empty allowlist.
---@param key string
---@return table rule
local function ruleFor(key)
    local rule = rules[key]
    if type(rule) ~= "table" then
        fail(RC .. " has no files[\"" .. key .. "\"] rule")
    end
    return rule
end

-- (c) the type checker's global list
local luarcGlobals = readLuarcGlobals(LUARC_PATH)

-- (d) manifest -> lint files: every game name must be allowed for adapters
local adaptersAllowed = allowedGlobals(ruleFor(ADAPTERS_RULE), RC)
for _, name in ipairs(sortedKeys(rcTop)) do
    adaptersAllowed[name] = true -- a files[...] list is merged with the top-level one
end
local mustAllow = {}
for name in pairs(manifestGlobals) do
    mustAllow[name] = true
end
mustAllow[PUBLIC_GLOBAL] = true
for _, name in ipairs(sortedKeys(mustAllow)) do
    if not adaptersAllowed[name] then
        addProblem(RC, nil, ("%s is in the compat manifest but not allowed in files[%q]"):format(name, ADAPTERS_RULE))
    end
    if not luarcGlobals[name] then
        addProblem(LUARC, nil, ("%s is in the compat manifest but missing from diagnostics.globals"):format(name))
    end
end

-- (d) lint files -> manifest: nothing else may be allowed
---@param name string
---@param where string
local function checkAllowedName(name, where)
    if not manifestGlobals[name] and not TOOLING[name] and name ~= PUBLIC_GLOBAL then
        addProblem(where, nil, name .. " is allowed but is neither in the compat manifest nor tooling")
    end
end

for _, name in ipairs(sortedKeys(rcTop)) do
    checkAllowedName(name, RC)
end
for _, key in ipairs(sortedKeys(rules)) do
    local rule = rules[key]
    local where = RC .. " files[" .. tostring(key) .. "]"
    if type(rule) ~= "table" then
        addProblem(RC, nil, "files[" .. tostring(key) .. "] is not a table")
    else
        if rule.std ~= nil then
            addProblem(where, nil, "overrides std; only the top-level std may set it")
        end
        for _, name in ipairs(sortedKeys(allowedGlobals(rule, where))) do
            checkAllowedName(name, where)
        end
    end
end
for _, name in ipairs(sortedKeys(luarcGlobals)) do
    checkAllowedName(name, LUARC)
end

-- (d) Core allows no global at all
local coreAllowed = allowedGlobals(ruleFor(CORE_RULE), RC)
for _, name in ipairs(sortedKeys(rcTop)) do
    coreAllowed[name] = true
end
for _, name in ipairs(sortedKeys(coreAllowed)) do
    addProblem(RC, nil, ("%s is allowed in Core, which must allow no global"):format(name))
end

-- (e) the TOC-loaded sources
local coreForbidden = {}
for name in pairs(manifestGlobals) do
    coreForbidden[name] = true
end
coreForbidden[PUBLIC_GLOBAL] = true
coreForbidden[PURE_GLOBAL] = true

local filesScanned = 0
local eventsSeen, namespacesSeen = {}, {}
for _, entry in ipairs(entries) do
    if entry ~= MANIFEST_ENTRY and entry:match("%.lua$") then
        local path = addonDir .. "/" .. entry
        local text = readFile(path, "cannot read " .. path)
        filesScanned = filesScanned + 1

        local where = ADDON .. "/" .. entry
        local code, literals = stripCommentsAndCollect(text)

        -- Flags a C_ namespace: the auction house one always, the others
        -- unless a manifest tier lists them.
        ---@param namespace string
        ---@param at number
        local function checkNamespace(namespace, at)
            namespacesSeen[namespace] = true
            if namespace == FORBIDDEN_NAMESPACE then
                addProblem(where, at, FORBIDDEN_NAMESPACE .. " must not appear anywhere in this addon")
            elseif not manifestGlobals[namespace] then
                addProblem(where, at, namespace .. " is used but not listed in the compat manifest")
            end
        end

        for _, literal in ipairs(literals) do
            local value = literal.value
            if isEventLiteral(value) then
                eventsSeen[value] = true
                if not manifestEvents[value] then
                    addProblem(
                        where,
                        literal.line,
                        ("event [%s] is used but not listed in the compat manifest"):format(value)
                    )
                end
            end
            for namespace in value:gmatch(NAMESPACE_PATTERN) do
                checkNamespace(namespace, literal.line)
            end
        end
        scanCaptures(code, NAMESPACE_PATTERN, function(start, namespace)
            checkNamespace(namespace, lineOf(code, start))
        end)

        if entry:sub(1, #CORE_PREFIX) == CORE_PREFIX then
            for _, name in ipairs(sortedKeys(coreForbidden)) do
                local token = "%f[%w_]" .. escapePattern(name) .. "%f[^%w_]"
                local declared = firstLocalDeclaration(code, name)
                scanCaptures(code, token, function(start)
                    -- The file's own local, from its declaration on: not a global.
                    if declared ~= nil and start >= declared then
                        return
                    end
                    addProblem(where, lineOf(code, start), name .. " is used in Core, which may touch no game global")
                end)
            end
        end
    end
end

if #problems > 0 then
    io.stderr:write(("compat manifest drift: %d problem(s)\n"):format(#problems))
    for _, problem in ipairs(problems) do
        io.stderr:write("  " .. problem .. "\n")
    end
    os.exit(1)
end

local summary = "compat manifest ok: %d file(s) scanned; %d manifest global(s) and %d event(s) "
    .. "allowed in %s and %s; %d event literal(s) and %d C_ namespace(s) in use, all listed"
print(
    summary:format(
        filesScanned,
        #sortedKeys(manifestGlobals),
        #sortedKeys(manifestEvents),
        RC,
        LUARC,
        #sortedKeys(eventsSeen),
        #sortedKeys(namespacesSeen)
    )
)
