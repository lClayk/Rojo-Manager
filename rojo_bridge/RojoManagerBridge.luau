--!strict
-- SPDX-License-Identifier: MPL-2.0
-- Rojo Manager bridge for the Rojo Studio plugin (https://github.com/rojo-rbx/rojo).
--
-- patch_rojo_plugin.luau adds this module to the official Rojo plugin on the user's machine.
-- In Edit mode it polls Rojo Manager on a fixed local port without pause. Each poll reports the
-- place, the server Rojo is connected to, and Rojo's status, so Rojo Manager shows Connected only for
-- real sessions. Rojo Manager holds the poll until it has a command for this Studio, or a few seconds.
--
-- The answer names the server of a running project with auto-connect linked to this place; the
-- bridge connects when that offer appears, accepting Rojo's initial sync prompt unless Rojo
-- Manager's settings say to ask. A command is one of the actions below; its result goes back with
-- the next poll. Commands only come from Rojo Manager, which takes them from agents through its
-- token-protected API.
-- It also tells Rojo Manager how many Rojo plugins are running, so it can warn about a second one.

local HttpService = game:GetService("HttpService")
local RunService = game:GetService("RunService")

local VERSION = 7
local POLL_URL = "http://localhost:34871/bridge/"
local REPORT_URL = "http://localhost:34871/rojo-plugins/"
-- How long to wait before trying again when Rojo Manager does not answer.
local RETRY_SECONDS = 5
-- The largest poll Rojo Manager accepts, results included.
local MAX_POLL_BYTES = 1024 * 1024
-- Rojo's port when its port field is empty.
local DEFAULT_PORT = 34872

-- The attached app's current poll loop, so a detached or reattached app's loop stops.
local attached: { [any]: {} } = {}

-- Rojo's live session, if any.
local function connected(app): boolean
	return app.serveSession ~= nil and app.serveSession:getStatus() == "Connected"
end

local function status(app)
	local state = app.state or {}
	-- appStatus is the page Rojo shows; its Settings page can sit over a session.
	local appStatus = state.appStatus
	if appStatus == "Settings" then
		appStatus = if connected(app) then "Connected" else "NotConnected"
	end
	return {
		bridge = VERSION,
		status = appStatus,
		host = app.host:getValue(),
		port = app.port:getValue(),
		projectName = state.projectName,
		address = state.address,
		-- Rojo keeps the last error message after reconnecting; only report a current one.
		error = if state.appStatus == "Error" then state.errorMessage else nil,
	}
end

local function answer(app, choice: string)
	if (app.state or {}).appStatus ~= "Confirming" then
		error("Rojo is not waiting for a sync confirmation", 0)
	end
	app.confirmationBindable:Fire(choice)
	return status(app)
end

-- End the current session. A session waiting for its sync confirmation is aborted first, so its
-- waiting thread does not resume and apply its sync when a later session is accepted.
local function endSession(app)
	if (app.state or {}).appStatus == "Confirming" then
		app.confirmationBindable:Fire("Abort")
	end
	if app.serveSession ~= nil then
		app:endSession()
	end
end

-- A value as JSON can carry it; anything else (an Instance, a function) as its text.
local function encodable(value: any): any
	local ok = pcall(HttpService.JSONEncode, HttpService, { value })
	return if ok then value else tostring(value)
end

local actions = {
	status = status,
	-- Connect Rojo to the Rojo server on this computer at args.port.
	connect = function(app, args)
		endSession(app)
		app.setHost("localhost")
		app.setPort(tostring(args.port))
		-- Starting can yield on the sync lock; return so the caller can poll status.
		task.spawn(app.startSession, app)
		return status(app)
	end,
	disconnect = function(app)
		endSession(app)
		return status(app)
	end,
	accept = function(app)
		return answer(app, "Accept")
	end,
	abort = function(app)
		return answer(app, "Abort")
	end,
	-- Run args.code with plugin permissions. Returns what it printed and returned.
	luau = function(_app, args)
		local chunk, compileError = loadstring(args.code, "=execute_luau")
		if not chunk then
			error(compileError, 0)
		end
		local output = {}
		local function capture(prefix: string)
			return function(...)
				local parts = {}
				for index = 1, select("#", ...) do
					parts[index] = tostring((select(index, ...)))
				end
				table.insert(output, prefix .. table.concat(parts, " "))
			end
		end
		setfenv(chunk, setmetatable({ print = capture(""), warn = capture("warning: ") }, { __index = getfenv() }))
		local results = table.pack(pcall(chunk))
		if not results[1] then
			local printed = if #output > 0 then "\nOutput:\n" .. table.concat(output, "\n") else ""
			error(tostring(results[2]) .. printed, 0)
		end
		local returned = {}
		for index = 2, results.n do
			returned[index - 1] = encodable(results[index])
		end
		return {
			success = true,
			returnValue = if results.n <= 2 then returned[1] else returned,
			output = output,
		}
	end,
}

-- Run one command from Rojo Manager; the result travels with the next poll.
local function run(app, command)
	local handler = actions[command.action]
	if not handler then
		return { id = command.id, ok = false, value = `Unknown Rojo Manager bridge action: {command.action}` }
	end
	local ok, value = pcall(handler, app, command.args or {})
	return { id = command.id, ok = ok, value = if ok then value else tostring(value) }
end

-- The server Rojo has a live session with, or 0.
local function connectedPort(app): number
	if not connected(app) then
		return 0
	end
	return tonumber(app.port:getValue()) or DEFAULT_PORT
end

-- Answer the sync prompt of the session auto-connect just started, once Rojo asks.
local function acceptPrompt(app)
	local deadline = os.clock() + 30
	while os.clock() < deadline do
		local status = (app.state or {}).appStatus
		if status == "Confirming" then
			app.confirmationBindable:Fire("Accept")
			return
		elseif connected(app) or status == "Error" then
			return -- no prompt this time, or the connection failed
		end
		task.wait(0.2)
	end
end

-- Poll until the plugin unloads. An auto-connect offer connects only when it appears (the place
-- opened, the server started, or auto-connect was turned on), so disconnecting by hand sticks.
local function poll(app, token: {})
	local id = Random.new():NextInteger(1, 2 ^ 31 - 1)
	local offered: number? = nil
	local result = nil
	while attached[app] == token do
		local started = os.clock()
		local report = { placeId = game.PlaceId, port = connectedPort(app), rojo = status(app), result = result }
		local encoded, json = pcall(HttpService.JSONEncode, HttpService, report)
		if result and (not encoded or #json > MAX_POLL_BYTES) then
			local problem = if encoded
				then `The result is too large to send ({#json} bytes). Return less.`
				else `The result could not be sent: {json}`
			report.result = { id = result.id, ok = false, value = problem }
			json = HttpService:JSONEncode(report)
		end
		local ok, response = pcall(HttpService.RequestAsync, HttpService, {
			Url = `{POLL_URL}{id}`,
			Method = "POST",
			Headers = { ["Content-Type"] = "application/json" },
			Body = json,
		})
		result = nil
		local body = nil
		if ok and response.Success then
			local decoded, value = pcall(HttpService.JSONDecode, HttpService, response.Body)
			body = if decoded and type(value) == "table" then value else nil
		end
		local offer: number? = if body and type(body.port) == "number" then body.port else nil
		-- No session, whatever page Rojo shows (Error after a lost connection, or Settings).
		if offer ~= nil and offer ~= offered and app.serveSession == nil then
			actions.connect(app, { port = offer })
			if body and body.accept == true then
				task.spawn(acceptPrompt, app)
			end
		end
		offered = offer
		if body and type(body.command) == "table" then
			result = run(app, body.command)
		elseif os.clock() - started < 1 then
			-- Rojo Manager is not running, or did not hold the poll.
			task.wait(RETRY_SECONDS)
		end
	end
end

-- Each running Rojo plugin opens one "Rojo <version>" widget.
local function countRojoPlugins(): number
	local count = 0
	for _, gui in game:GetService("PluginGuiService"):GetChildren() do
		if gui:IsA("DockWidgetPluginGui") and string.match(gui.Name, "^Rojo %d") then
			count += 1
		end
	end
	return count
end

local function reportRojoPlugins()
	-- Other plugins finish loading after this one; give them time to open their widgets.
	task.wait(5)
	pcall(HttpService.RequestAsync, HttpService, {
		Url = REPORT_URL .. countRojoPlugins(),
		Method = "GET",
	})
end

local Bridge = {}

-- Called from App:init. Only the Edit-mode plugin is controllable.
function Bridge.attach(app)
	if not RunService:IsEdit() then
		return
	end
	local token = {}
	attached[app] = token
	-- Deferred until App:init has finished, so the host and port the poll reports are set.
	task.defer(poll, app, token)
	task.spawn(reportRojoPlugins)
end

-- Called from App:willUnmount when the plugin unloads.
function Bridge.detach(app)
	attached[app] = nil
end

return Bridge
