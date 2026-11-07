--!strict
-- SPDX-License-Identifier: MPL-2.0
-- Rojo Manager bridge for the Rojo Studio plugin (https://github.com/rojo-rbx/rojo).
--
-- patch_rojo_plugin.luau adds this module to the official Rojo plugin on the user's machine.
-- In Edit mode it publishes CoreGui.RojoManagerBridge, a BindableFunction that plugin code
-- (such as robloxstudio-mcp's execute_luau) can invoke. Game scripts cannot reach CoreGui.
--
--   bridge:Invoke("status")              -> { status, host, port, projectName, address, error }
--   bridge:Invoke("connect", host, port) -> status; connects in the background
--   bridge:Invoke("disconnect")          -> status
--   bridge:Invoke("accept" | "abort")    -> status; answers Rojo's initial sync confirmation
--
-- Every few seconds it tells Rojo Manager (on a fixed local port) which server Rojo is connected to,
-- so Rojo Manager shows Connected only for real sessions. The answer names the server of a running
-- project with auto-connect linked to this place, and the bridge connects when that offer appears.
-- It also tells Rojo Manager how many Rojo plugins are running, so it can warn about a second one.

local CoreGui = game:GetService("CoreGui")
local HttpService = game:GetService("HttpService")
local RunService = game:GetService("RunService")

local NAME = "RojoManagerBridge"
local VERSION = 6
local POLL_URL = "http://localhost:34871/bridge/"
local REPORT_URL = "http://localhost:34871/rojo-plugins/"
local POLL_SECONDS = 2
-- Rojo's port when its port field is empty.
local DEFAULT_PORT = 34872

local published: { [any]: BindableFunction } = {}

local function status(app)
	local state = app.state or {}
	return {
		bridge = VERSION,
		status = state.appStatus,
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

local actions = {
	status = status,
	connect = function(app, host: string?, port: string?)
		-- Any plugin can call this bridge, so it only connects Rojo to servers on this computer.
		if host ~= nil and host ~= "" and host ~= "localhost" and host ~= "127.0.0.1" then
			error("Rojo Manager's bridge only connects to Rojo servers on this computer", 0)
		end
		endSession(app)
		app.setHost(host or "")
		app.setPort(port or "")
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
}

-- The server Rojo has a live session with, or 0.
local function connectedPort(app): number
	if (app.state or {}).appStatus ~= "Connected" then
		return 0
	end
	return tonumber(app.port:getValue()) or DEFAULT_PORT
end

-- Report the session and act on the auto-connect offer until the plugin unloads. An offer connects
-- only when it appears (the place opened, the server started, or auto-connect was turned on), so
-- disconnecting by hand sticks.
local function poll(app, bindable: BindableFunction)
	local id = Random.new():NextInteger(1, 2 ^ 31 - 1)
	local offered: number? = nil
	while published[app] == bindable do
		local ok, response = pcall(HttpService.RequestAsync, HttpService, {
			Url = `{POLL_URL}{id}/{game.PlaceId}/{connectedPort(app)}`,
			Method = "GET",
		})
		-- 404 means Rojo Manager knows no running project that auto-connects here.
		local offer: number? = nil
		if ok and response.Success then
			local decoded, body = pcall(HttpService.JSONDecode, HttpService, response.Body)
			if decoded and type(body) == "table" and type(body.port) == "number" then
				offer = body.port
			end
		end
		local idle = app.serveSession == nil and (app.state or {}).appStatus == "NotConnected"
		if offer ~= nil and offer ~= offered and idle then
			actions.connect(app, "localhost", tostring(offer))
		end
		offered = offer
		task.wait(POLL_SECONDS)
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
	local previous = CoreGui:FindFirstChild(NAME)
	if previous then
		previous:Destroy()
	end
	local bindable = Instance.new("BindableFunction")
	bindable.Name = NAME
	bindable.Archivable = false
	bindable.OnInvoke = function(action: string, ...)
		local handler = actions[action]
		if not handler then
			error(`Unknown Rojo Manager bridge action: {tostring(action)}`, 0)
		end
		return handler(app, ...)
	end
	bindable.Parent = CoreGui
	published[app] = bindable
	-- The first request yields, so the app has mounted by the time it can connect.
	task.spawn(poll, app, bindable)
	task.spawn(reportRojoPlugins)
end

-- Called from App:willUnmount when the plugin unloads.
function Bridge.detach(app)
	local bindable = published[app]
	if bindable then
		published[app] = nil
		bindable:Destroy()
	end
end

return Bridge
