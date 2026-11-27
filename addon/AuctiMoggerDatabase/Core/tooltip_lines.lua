local _, A = ...
A.Core = A.Core or {}

-- Core/tooltip_lines.lua - the tooltip's rows (pure Lua).
--
-- One decoded record and a small context become the rows the attached frame
-- renders (SPEC 7.6): a header, the three basics, and then either the
-- optional lines of the standard level or, with Shift held, the detail lines.
-- A row is built from the data it needs and left out when that data is
-- absent; an absent field is never printed as a zero. Colors are escape codes
-- inside the text, and the frame adds the rows and draws nothing else.
-- Nothing here reads a game global or a clock: the ages and the settings
-- arrive in the context.

---@type AmdbFormatModule
local Format = A.Core.Format
--- Bound under its own name, not the adapters' `Preferences`: Core never
--- calls the client's Settings API, and the compat guard reads a manifest
--- name in Core as the global unless the file declares a local of that name.
---@type AmdbSettingsModule
local Settings = A.Core.Settings

-- The spans in wire order, which is the display order.
local SPAN_NAMES = { "d1", "d7", "d14", "d30" }

-- The saturation value of `supplyDays` (SPEC 5.7): at least that many days.
local SUPPLY_SATURATED = 65535

---@class AmdbTooltipLinesModule
---@field build fun(record: AmdbRecord, context: AmdbTipContext): AmdbTipRow[]
local TooltipLines = {}
A.Core.TooltipLines = TooltipLines

--- The record of the same item in another loaded market.
---@class AmdbTipOther
---@field target string that market's target id
---@field record AmdbRecord its record of the item

--- What the rows are built against. The adapter reads the store and the clock
--- and fills it in; the builder only reads it. An age is `now` minus a stamp,
--- so a clock a little ahead reads as a negative age and formats as zero.
---@class AmdbTipContext
---@field itemID integer|nil the item id; nil for a battle pet
---@field speciesID integer|nil the species id; nil for an item
---@field category AmdbCategory the record table the record was read from
---@field target string the primary market's target id
---@field marketAge number now minus the market's generatedAt
---@field auctionsAge number|nil now minus auctionsAt; nil when the header lacks the stamp
---@field commoditiesAge number|nil now minus commoditiesAt; nil when the header lacks the stamp
---@field feedVersion integer the carrier's contract version
---@field others AmdbTipOther[] the other loaded markets that have the item, in slot order
---@field collapsed boolean the owner tooltip is collapsed: the three basics only
---@field shift boolean Shift is held and the settings allow the detail level
---@field settings AmdbSettings the live settings: which optional lines are on

--- One row of the attached frame (SPEC 7.6).
---@class AmdbTipRow
---@field left string the label; the caption of a sparkline row
---@field right string|nil nil: a single line, left-aligned
---@field bars integer[]|nil present: a sparkline beside the label, one byte a day (0..255), oldest first

------------------------------------------------------------------ text parts

---@param text string
---@return string dim
local function dim(text)
    return Format.colorize("dim", text)
end

--- A ranking score in basis points as "0.71", by integer math: the same two
--- digits on every client, which a float format is not.
---@param bp integer
---@return string text
local function scoreText(bp)
    local hundredths = math.floor(bp / 100 + 0.5)
    return ("%d.%02d"):format(math.floor(hundredths / 100), hundredths % 100)
end

--- `value` against `base`, in basis points and rounded half away from zero;
--- nil without a base to compare with.
---@param value number|nil
---@param base number|nil
---@return integer|nil bp
local function differenceBp(value, base)
    if value == nil or base == nil or base <= 0 then
        return nil
    end
    local scaled = (value - base) / base * 10000
    if scaled >= 0 then
        return math.floor(scaled + 0.5)
    end
    return math.ceil(scaled - 0.5)
end

--- A craft result in money with its sign; a loss reads in bad.
---@param copper integer signed copper
---@return string text
local function resultText(copper)
    if copper < 0 then
        return Format.colorize("bad", Format.moneySigned(copper))
    end
    return Format.moneySigned(copper) or ""
end

-------------------------------------------------------------- the three basics

--- The header: the addon title, and the market with the age of the record's
--- own snapshot (SPEC 7.5.4). A commodity trades in the commodity house, so
--- its data is as old as that snapshot; everything else sells in the
--- auctions. Without that stamp the market's own age stands in.
---@param context AmdbTipContext
---@return AmdbTipRow row
local function headerRow(context)
    local age
    if context.category == "commodity" then
        age = context.commoditiesAge
    else
        age = context.auctionsAge
    end
    if age == nil then
        age = context.marketAge
    end
    return {
        left = Format.PREFIX,
        right = context.target .. " · " .. Format.colorize(Format.ageColor(age), Format.age(age)),
    }
end

--- Min buyout: the live floor with the quantity at it, or why there is none.
---@param record AmdbRecord
---@return AmdbTipRow row
local function buyoutRow(record)
    local live = record.live
    local text
    if live ~= nil and live.floor ~= nil then
        text = Format.money(live.floor) .. dim("  ×" .. Format.count(live.floorQty))
        if record:atVendorFloor() then
            text = text .. Format.colorize("good", "  ≤ vendor")
        end
    elseif live ~= nil and live.seenFloor ~= nil then
        text = dim("none listed · seen today " .. Format.money(live.seenFloor) .. " ×" .. Format.count(live.seenQty))
    else
        text = dim("not listed")
    end
    return { left = "Min buyout", right = text }
end

--- Market value: the reference value, where it comes from, and the trend.
---@param record AmdbRecord
---@return AmdbTipRow row
local function valueRow(record)
    local value = record:reference()
    local text
    if value == nil then
        text = dim("unknown")
    else
        text = Format.money(value) .. dim("  " .. (record:valueSource() or ""))
        local trend = record.trendBp
        if trend ~= nil then
            text = text .. Format.colorize(Format.trendTier(trend), "  " .. Format.signedPercentBp(trend))
        end
    end
    return { left = "Market value", right = text }
end

--- Sale rate: the 14-day rate with the closures behind it; the 180-day rate,
--- under its own name, when there is no 14-day one. The counts describe the
--- fortnight, so they are not shown beside the longer window.
---@param record AmdbRecord
---@return AmdbTipRow row
local function rateRow(record)
    local rate = record:saleRate14()
    local long = false
    if rate == nil then
        local longer = record:saleRate180d()
        if longer ~= nil then
            rate = longer
            long = true
        end
    end
    local text
    if rate == nil then
        text = dim("no evidence")
    else
        text = Format.colorize(Format.rateTier(rate), Format.percent(rate))
        local sold = record.soldCount
        local expired = record.expiredCount
        if not long and sold ~= nil and expired ~= nil then
            text = text .. dim("  (" .. Format.count(sold) .. " of " .. Format.count(sold + expired) .. " sold)")
        end
    end
    return { left = long and "Sale rate (180d)" or "Sale rate (14d)", right = text }
end

--- The header and the three basics: what every level shows.
---@param record AmdbRecord
---@param context AmdbTipContext
---@return AmdbTipRow[] rows
local function basics(record, context)
    return { headerRow(context), buyoutRow(record), valueRow(record), rateRow(record) }
end

--------------------------------------------------- the optional lines, standard

--- A number of days as a when: "today", "1d ago", "Nd ago".
---@param days integer whole days
---@return string text
local function ago(days)
    if days <= 0 then
        return "today"
    end
    if days == 1 then
        return "1d ago"
    end
    return Format.count(days) .. "d ago"
end

---@param record AmdbRecord
---@return AmdbTipRow|nil row
local function soldRow(record)
    local perDay = record:soldPerDay()
    if perDay == nil then
        return nil
    end
    local text = Format.rate(perDay) .. "/day"
    if record.supplyDays ~= nil or record:supplyUnbounded() then
        text = text .. " · supply " .. Format.supplyDays(record.supplyDays)
    end
    return { left = "Sold", right = text }
end

---@param record AmdbRecord
---@return AmdbTipRow|nil row
local function lastSaleRow(record)
    local days = record:daysSinceLastSale()
    if days == nil then
        return nil
    end
    return { left = "Last sale", right = ago(days) }
end

--- Last listed: how many days ago the item was last seen listed, which is
--- another question than when it last sold. The tracker dates it off the
--- newest snapshot and the hour fold (SPEC 5.6).
---@param record AmdbRecord
---@return AmdbTipRow|nil row
local function listedRow(record)
    local live = record.live
    local days = live ~= nil and live.daysSinceListed or nil
    if days == nil then
        return nil
    end
    return { left = "Last listed", right = ago(days) }
end

---@param record AmdbRecord
---@return AmdbTipRow|nil row
local function lowRow(record)
    local low30 = record.low30
    if low30 == nil then
        return nil
    end
    local text = Format.money(low30)
    if record.lowRank30d ~= nil then
        text = text .. dim("  · " .. Format.percentBp(record.lowRank30d) .. " of days higher")
    end
    return { left = "30-day low", right = text }
end

---@param record AmdbRecord
---@return AmdbTipRow|nil row
local function sparklineRow(record)
    local lows = record.lows
    if lows == nil or #lows == 0 then
        return nil
    end
    return { left = "30 days", right = nil, bars = lows }
end

---@param record AmdbRecord
---@return AmdbTipRow|nil row
local function craftRow(record)
    local craft = record.craft
    if craft == nil then
        return nil
    end
    local text = Format.money(craft.cost)
        .. " → "
        .. resultText(craft.profit)
        .. dim("  (exp. ")
        .. resultText(craft.expectedProfit)
        .. dim(")")
    return { left = "Crafting", right = text }
end

---@param record AmdbRecord
---@return AmdbTipRow|nil row
local function dealRow(record)
    local deal = record.deal
    if deal == nil then
        return nil
    end
    local text = Format.colorize(
        "good",
        "#"
            .. Format.count(deal.rank)
            .. " · "
            .. Format.percentBp(deal.discountBp)
            .. " under value · score "
            .. scoreText(deal.scoreBp)
    )
    return { left = "Deal", right = text }
end

--- One line per other market that has the item: what it is worth there, and
--- how that differs from the primary market.
---@param record AmdbRecord the primary market's record
---@param context AmdbTipContext
---@param rows AmdbTipRow[] appended to
local function addOtherMarkets(record, context, rows)
    local base = record:reference()
    for index = 1, #context.others do
        local other = context.others[index]
        local value = other.record:reference()
        if value ~= nil then
            local text = Format.money(value)
            local difference = differenceBp(value, base)
            if difference ~= nil then
                text = text .. "  " .. Format.signedPercentBp(difference)
            end
            rows[#rows + 1] = { left = Format.colorize("muted", other.target), right = text }
        end
    end
end

--- The standard level: the basics, then every optional line that is on and
--- has its data, in display order.
---@param record AmdbRecord
---@param context AmdbTipContext
---@return AmdbTipRow[] rows
local function standardRows(record, context)
    local rows = basics(record, context)
    local settings = context.settings
    ---@param key string the optional line's key
    ---@param build fun(record: AmdbRecord): AmdbTipRow|nil
    local function optional(key, build)
        if Settings.isLineOn(settings, key) then
            local row = build(record)
            if row ~= nil then
                rows[#rows + 1] = row
            end
        end
    end
    optional("sold", soldRow)
    optional("lastSale", lastSaleRow)
    optional("listed", listedRow)
    optional("low", lowRow)
    optional("sparkline", sparklineRow)
    optional("craft", craftRow)
    optional("deal", dealRow)
    if Settings.isLineOn(settings, "markets") then
        addOtherMarkets(record, context, rows)
    end
    return rows
end

------------------------------------------------------- the lines Shift shows

--- Adds a line with a right-hand value; nothing when the value is absent.
---@param rows AmdbTipRow[]
---@param label string
---@param text string|nil
local function addLine(rows, label, text)
    if text ~= nil then
        rows[#rows + 1] = { left = label, right = text }
    end
end

--- The detail level (SPEC 7.6): the basics, then the evidence behind them in
--- place of the optional lines, one line each, each left out when its data
--- is absent.
---@param record AmdbRecord
---@param context AmdbTipContext
---@return AmdbTipRow[] rows
local function detailRows(record, context)
    local rows = basics(record, context)
    local live = record.live
    local qtySum = live ~= nil and live.qtySum or nil
    local perDay = record:soldPerDay()

    if record.soldMedian14 ~= nil then
        addLine(rows, "Sold median (14d)", Format.money(record.soldMedian14))
    end

    if record.value7 ~= nil then
        addLine(
            rows,
            "Value basis",
            "7-day listing value" .. dim("  · " .. Format.count(record.samples7) .. " listings")
        )
    end

    if live ~= nil and live.listed ~= nil then
        local text = Format.count(live.listed) .. " listings"
        if qtySum ~= nil then
            text = text .. dim("  · " .. Format.count(qtySum) .. " units")
        end
        addLine(rows, "Listed", text)
    end

    addLine(rows, "Vendor sell", record.vendorPrice ~= nil and Format.money(record.vendorPrice) or nil)

    -- Supply is the listed quantity divided by the daily pace (SPEC 5.6), so
    -- the line says so with the two numbers the tracker divided.
    local supplyDays = record.supplyDays
    if supplyDays ~= nil then
        local text
        if supplyDays == SUPPLY_SATURATED then
            text = "at least " .. Format.count(supplyDays) .. " days"
        else
            text = Format.count(supplyDays) .. " days"
        end
        if qtySum ~= nil and perDay ~= nil then
            text = text .. dim("  (" .. Format.count(qtySum) .. " listed ÷ " .. Format.rate(perDay) .. "/day)")
        end
        addLine(rows, "Supply", text)
    end

    -- The basic line already shows the 180-day rate when there is no 14-day
    -- one: this line adds the longer view, it does not repeat the basic.
    if record:saleRate14() ~= nil then
        local rate = record:saleRate180d()
        if rate ~= nil then
            addLine(rows, "Sale rate (180d)", Format.colorize(Format.rateTier(rate), Format.percent(rate)))
        end
    end

    local closures = record:closuresSinceLastSale()
    if closures ~= nil then
        addLine(rows, "Closures since last sale", Format.count(closures))
    end

    local snapshots = {}
    if context.auctionsAge ~= nil then
        snapshots[#snapshots + 1] = "auctions " .. Format.age(context.auctionsAge)
    end
    if context.commoditiesAge ~= nil then
        snapshots[#snapshots + 1] = "commodities " .. Format.age(context.commoditiesAge)
    end
    if #snapshots > 0 then
        addLine(rows, "Snapshots", table.concat(snapshots, " · ") .. " ago")
    end

    for index = 1, #SPAN_NAMES do
        local name = SPAN_NAMES[index]
        local block = record.spans[name]
        if block ~= nil then
            -- The name column is as wide as the longest span name plus a
            -- space, so the medians line up.
            local text = "p05 "
                .. Format.money(block.p05)
                .. " · p95 "
                .. Format.money(block.p95)
                .. " · "
                .. Format.count(block.n)
                .. " seen"
            if block.qtySold ~= nil then
                text = text .. " · " .. Format.count(block.qtySold) .. " sold"
            end
            rows[#rows + 1] = {
                left = name .. (" "):rep(5 - #name) .. "median " .. Format.money(block.median),
                right = text,
            }
        end
    end

    local id = context.itemID or context.speciesID
    if id ~= nil then
        rows[#rows + 1] = {
            left = dim(tostring(id) .. " · " .. context.category .. " · feed v" .. tostring(context.feedVersion)),
            right = nil,
        }
    end
    return rows
end

------------------------------------------------------------------------ build

--- The rows of one tooltip (SPEC 7.6). A collapsed owner always gets the
--- three basics only; Shift held gets the detail level; otherwise the
--- standard level. Never empty: the header and the basics are always there.
---@param record AmdbRecord the record the tooltip is about
---@param context AmdbTipContext
---@return AmdbTipRow[] rows
function TooltipLines.build(record, context)
    if context.collapsed then
        return basics(record, context)
    end
    if context.shift then
        return detailRows(record, context)
    end
    return standardRows(record, context)
end

return TooltipLines
