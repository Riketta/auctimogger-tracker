local _, A = ...

-- bootstrap.lua - the composition root.
--
-- Loads last (TOC order) and wires what the earlier files published on the
-- namespace into the game. This first version owns one frame and one event.
-- PLAYER_LOGIN is the first moment the player can read chat, so it is where
-- the status line of SPEC 7.11 and every report the slot files recorded while
-- they loaded are printed; the update callbacks run right after, so
-- subscribers see the markets. The SavedVariables lifecycle, the tooltip hooks
-- and the options panel arrive with their own adapters.
--
-- The status line is a pure function of the store and `now`: a spec reads it
-- through a stubbed frame, without a client.

---@type AmdbFormatModule
local Format = A.Core.Format

--- The slice of the client's Frame this file calls. The client's frame type is
--- declared nowhere in this workspace, so the methods used are listed here.
---@class AmdbEventFrame
---@field RegisterEvent fun(self: AmdbEventFrame, eventName: string)
---@field SetScript fun(self: AmdbEventFrame, scriptType: string, handler: fun(self: AmdbEventFrame, event: string))

--- SPEC 7.11, for a player who has no feed yet: the line says what to do.
local NO_DATA = "no market data — copy the tracker's feed.lua over Market1.lua"

--- SPEC 7.11, for several markets and no choice.
local PICK_ONE = "several markets loaded, pick one: /amdb market <id>"

--- SPEC 7.11, for a market other than the primary that is past 24 hours:
--- shown rather than hidden (SPEC 5.9), so the user knows to refresh it.
local RESYNC = " old — resync"

-- The house color of each age tier. The formatter's "stale" tier is the red
-- SPEC 7.5.4 gives stale data, which the color table keeps under "bad".
---@type table<AmdbAgeTier, AmdbColorName>
local AGE_COLORS = {
    muted = "muted",
    warn = "warn",
    hot = "hot",
    stale = "bad",
}

--- The login status line (SPEC 7.11). Pure: it reads the store and `now`, and
--- nothing else. Stale markets other than the primary follow as a suffix
--- each, because the primary's own age is already in the line.
---@param store AmdbStore
---@param now number epoch seconds
---@return string line
local function statusLine(store, now)
    local primary = store:primary()
    if primary == nil then
        return Format.PREFIX .. ": " .. Format.colorize("bad", NO_DATA)
    end

    local markets = store:markets()
    local age = primary:ageSeconds(now)
    local parts = {
        #markets .. (#markets == 1 and " market" or " markets"),
        "primary " .. primary.target,
        "data " .. Format.colorize(AGE_COLORS[Format.ageTier(age)], Format.age(age)) .. " old",
    }
    if store:primaryIsAmbiguous() then
        parts[#parts + 1] = PICK_ONE
    end
    for i = 1, #markets do
        local market = markets[i]
        if market ~= primary and market:isStale(now) then
            parts[#parts + 1] =
                Format.colorize("hot", market.target .. " is " .. Format.age(market:ageSeconds(now)) .. RESYNC)
        end
    end
    return Format.PREFIX .. ": " .. table.concat(parts, " · ")
end

--- Prints the reports recorded so far, one line each in the color of their
--- level, and clears them.
local function printReports()
    local reports = A.TakeReports()
    for i = 1, #reports do
        local report = reports[i]
        A.chat(A.FormatReport(report.text, report.level))
    end
end

--- The frame's OnEvent. Only PLAYER_LOGIN is registered in this version.
---@param _ AmdbEventFrame the frame the script runs on
---@param event string
local function onEvent(_, event)
    if event ~= "PLAYER_LOGIN" then
        return
    end
    A.chat(statusLine(A.store, A.clock()))
    printReports()
    -- The store keeps one failing callback from stopping the others; what it
    -- caught is reported here, once, in bad.
    local errors = A.store:fireUpdate()
    for i = 1, #errors do
        A.Say(errors[i], "bad")
    end
    if not A.verbose then
        -- Not printed by Say: login is the one moment these reach chat.
        printReports()
    end
end

---@type AmdbEventFrame
local frame = CreateFrame("Frame")
frame:RegisterEvent("PLAYER_LOGIN")
frame:SetScript("OnEvent", onEvent)

return A
