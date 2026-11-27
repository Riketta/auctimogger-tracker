-- AuctiMogger Database: lint + architecture boundary rules
-- (addon/SPEC.md sections 7.2 and 9).
--
-- Run luacheck FROM addon/ (`cd addon && luacheck .`): it reads this file
-- only from the current directory.
--
-- Boundary rule:
--   * AuctiMoggerDatabase/Core and the generated Market*.lua slots get NO
--     globals at all, not even _G: any game API reference there fails CI.
--   * AuctiMoggerDatabase/Adapters gets exactly the manifest published by
--     Adapters/compat.lua (required + optional + authored, the attached frame's
--     global included) plus
--     GetAuctionBuyout, the public global the addon defines. C_AuctionHouse
--     is deliberately absent: it appears nowhere in this addon.
--   * AuctiMoggerDatabase/bootstrap.lua (the composition root) gets the same
--     surface as the adapters.
--   * spec files get the busted harness globals plus the game globals they
--     stub to exercise the adapters.
--   * scripts get the globals run_tests.lua defines.
--
-- There is deliberately NO top-level `globals`: a files[...] list is MERGED
-- with it, so any name there would be allowed in Core too. A file that
-- matches no rule below therefore gets the Lua 5.1 standard library only.
-- luacheck's own lua51 std admits _G; the rules that must stay pure revoke it.
--
-- Every game name in GAME_FACING also lives in Adapters/compat.lua and in
-- .luarc.json: scripts/check_compat.lua diffs the three and fails on drift.
-- Add a game global in all three places, in the same change.

std = "lua51"
cache = false

-- LuaLS annotations are single-line by design (`@field fun(...)` types do
-- not wrap), so this limit sits above stylua's 120 code columns (which the
-- formatter enforces on its own).
max_line_length = 150
-- Builders and helpers legitimately ignore the implicit self of `function M:x()`.
self = false

-- Vendored toolchain trees: a local luarocks install lands in .luarocks,
-- the local LuaLS/StyLua distributions live under tools/.
exclude_files = { "tools/**", ".luarocks/**" }

-- The game-facing surface, in the manifest's tier order. This one table
-- feeds the adapter, bootstrap and spec rules so they cannot drift apart.
local GAME_FACING = {
	-- required
	"CreateFrame",
	"GameTooltip",
	"SlashCmdList",
	"UIParent",
	"time",
	-- optional
	"Enum",
	"GetMoneyString",
	"ItemRefTooltip",
	"IsShiftKeyDown",
	"MenuUtil",
	"Settings",
	"TooltipDataProcessor",
	"issecretvalue",
	"print",
	-- authored
	"AuctiMoggerDatabase",
	"AuctiMoggerDatabaseTip",
	"GetAuctionBuyout",
	"SLASH_AMDB1",
	"SLASH_AMDB2",
}

-- Pure Lua: the Lua 5.1 standard library and nothing else. The allowlist is
-- explicitly empty.
files["AuctiMoggerDatabase/Core"] = {
	globals = {},
	not_globals = { "_G" },
}

-- Generated data slots (local _, A = ... / A.BeginMarket). No globals, and no
-- line-length limit: the tracker writes one record string per line and a
-- record is longer than 150 columns by design.
files["AuctiMoggerDatabase/Market*"] = {
	globals = {},
	not_globals = { "_G" },
	max_line_length = false,
}

files["AuctiMoggerDatabase/Adapters"] = {
	globals = GAME_FACING,
}

-- The composition root creates the event frame itself and wires the adapters:
-- same surface as an adapter.
files["AuctiMoggerDatabase/bootstrap.lua"] = {
	globals = GAME_FACING,
}

-- Carried over from the sniper: a spec stub that types a WoW global may be
-- variadic on purpose, so 212 (unused vararg argument) is treated as noise
-- here. Every other check stays on.
local SPEC_GLOBALS = {
	"after_each",
	"before_each",
	"describe",
	"expose",
	"insulate",
	"it",
	"lazy_setup",
	"lazy_teardown",
	"match",
	"mock",
	"pending",
	"setup",
	"spy",
	"stub",
	"teardown",
	-- the Lua 5.1 standalone interpreter exposes it; specs read script paths
	"arg",
}
-- ...then the whole game-facing surface the specs stub.
for _, name in ipairs(GAME_FACING) do
	SPEC_GLOBALS[#SPEC_GLOBALS + 1] = name
end

files["spec"] = {
	ignore = { "212" },
	globals = SPEC_GLOBALS,
}

-- run_tests.lua defines these as globals for the spec files it loads (and
-- re-declares assert in an inline directive of its own).
files["scripts"] = {
	globals = {
		"after_each",
		"before_each",
		"describe",
		"it",
		"setup",
		"teardown",
	},
}
