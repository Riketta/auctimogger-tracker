-- Verifies a generated tracker feed (feed.lua) against the v3
-- contract the AuctiMogger addon ingests (see the tracker README's
-- "Feed contract (v3)" section).
-- Usage: lua verify_feed.lua <feed.lua>
-- Exits non-zero with a message on the first violation.
--
-- TRUST: this script executes the feed file as trusted Lua (loadfile +
-- pcall) — the same trust domain as the tracker itself. Only point it at
-- feeds you generated with the tracker; it must never be run against
-- untrusted files.

local path = assert(arg and arg[1], "usage: lua verify_feed.lua <feed.lua>")

local chunk = assert(loadfile(path), "feed file does not compile as Lua")
local ok = pcall(chunk)
assert(ok, "feed file errors at runtime")
-- The contract is a global (the addon reads it the same way).
local feed = AuctiMoggerTrackerFeed
assert(type(feed) == "table", "feed must be a table")
assert(feed.version == 3, "feed.version must be 3")
assert(type(feed.generatedAt) == "number" and feed.generatedAt > 0 and feed.generatedAt % 1 == 0,
	"feed.generatedAt must be a positive integer epoch")
assert(type(feed.items) == "table", "feed.items must be a table")

local SPANS = { "d1", "d7", "d14", "d30" }
local function isUInt(v)
	return type(v) == "number" and v >= 0 and v % 1 == 0
end

local function validSpan(span)
	if type(span) ~= "table" then
		return false
	end
	if not isUInt(span.n) or span.n < 1 then
		return false
	end
	if type(span.median) ~= "number" or span.median <= 0 then
		return false
	end
	for _, field in ipairs({ "min", "p05", "p25", "p75", "p95", "max", "mad", "vwap", "qtySum", "listingCount", "saleRate" }) do
		local v = span[field]
		if v ~= nil and (type(v) ~= "number" or v < 0) then
			return false
		end
	end
	local ordered = function(a, b)
		return a == nil or b == nil or a <= b
	end
	return ordered(span.min, span.p05)
		and ordered(span.p05, span.p25)
		and ordered(span.p25, span.median)
		and ordered(span.median, span.p75)
		and ordered(span.p75, span.p95)
		and ordered(span.p95, span.max)
end

local function fail(index, message)
	error(("item %d: %s"):format(index, message), 0)
end

local CATEGORIES = { "item", "commodity", "pet" }

local count = 0
for index, entry in ipairs(feed.items) do
	assert(type(entry) == "table", ("items[%d] must be a table"):format(index))
	local spans = type(entry.metrics) == "table" and entry.metrics or nil
	if not spans then
		fail(index, "missing metrics table")
	end
	-- v3: every entry names its market category (items/commodities/pets are
	-- separate ledgers; an item trading in both AHs exports once per one).
	local category = entry.category
	local knownCategory = false
	for _, known in ipairs(CATEGORIES) do
		if category == known then
			knownCategory = true
			break
		end
	end
	assert(knownCategory, ("items[%d] needs a category of item|commodity|pet"):format(index))
	local validSpans = 0
	for _, name in ipairs(SPANS) do
		local span = spans[name]
		if span ~= nil then
			assert(validSpan(span), ("items[%d].%s is not a valid span"):format(index, name))
			validSpans = validSpans + 1
		end
	end
	assert(validSpans > 0, ("items[%d] has no usable span"):format(index))

	if entry.kind == "pet" then
		assert(isUInt(entry.speciesID) and entry.speciesID > 0,
			("items[%d] pet needs a positive speciesID"):format(index))
		assert(category == "pet", ("items[%d] pet entries must use the pet category"):format(index))
	else
		assert(entry.kind == nil and isUInt(entry.itemID) and entry.itemID > 0,
			("items[%d] needs a positive itemID"):format(index))
	end
	count = count + 1
end

print(("feed ok: %d items, generatedAt %d"):format(count, feed.generatedAt))
