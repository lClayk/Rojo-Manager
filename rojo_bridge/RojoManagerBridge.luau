--!strict
-- SPDX-License-Identifier: MPL-2.0
-- Rojo Manager bridge for the Rojo Studio plugin (https://github.com/rojo-rbx/rojo).
--
-- patch_rojo_plugin.luau adds this module to the official Rojo plugin on the user's machine.
-- In Edit mode it polls Rojo Manager on a fixed local port without pause. Each poll reports the
-- place, the server Rojo is connected to, and Rojo's status, so Rojo Manager shows Connected only for
-- real sessions. An unpublished place (PlaceId 0) is named by its file name, which Studio gives
-- game.Name when it opens the file (see placeName). Rojo Manager holds the poll until it has a command for this Studio, or a few seconds.
--
-- The answer names the server of a running project with auto-connect linked to this place; the
-- bridge connects when that offer appears, accepting Rojo's initial sync prompt unless Rojo
-- Manager's settings say to ask. A command is one of the actions below; its result goes back with
-- the next poll. Commands only come from Rojo Manager, which takes them from agents through its
-- token-protected API.
-- While Rojo waits for its initial sync to be confirmed, the status carries a summary of what
-- accepting would do, so agents and the window see it before anyone accepts.
-- It also tells Rojo Manager how many Rojo plugins are running, so it can warn about a second one.
--
-- In Edit mode and in a playtest's server it also sends Studio's Output to Rojo Manager in small
-- batches as lines appear, so agents can read prints, warnings, and errors. A playtest's client
-- cannot make HTTP requests, so its Output stays in Studio.

local HttpService = game:GetService("HttpService")
local LogService = game:GetService("LogService")
local RunService = game:GetService("RunService")

local VERSION = 11
local POLL_URL = "http://localhost:34871/bridge/"
local REPORT_URL = "http://localhost:34871/rojo-plugins/"
local OUTPUT_URL = "http://localhost:34871/output"
local BACKUP_URL = "http://localhost:34871/backup"
-- The largest backup Rojo Manager accepts, base64 included.
local MAX_BACKUP_BYTES = 64 * 1024 * 1024
-- Marks each copy in a backup with the full name it had, so restore finds its parent.
local BACKUP_PATH_ATTRIBUTE = "RojoManagerPath"
-- How long new Output lines wait to be sent together, and how many wait at most.
local OUTPUT_DELAY = 0.5
local MAX_OUTPUT_LINES = 500
-- Characters sent per line; Rojo Manager keeps no more.
local MAX_OUTPUT_TEXT = 2000
local OUTPUT_KINDS = {
	[Enum.MessageType.MessageOutput] = "output",
	[Enum.MessageType.MessageInfo] = "info",
	[Enum.MessageType.MessageWarning] = "warning",
	[Enum.MessageType.MessageError] = "error",
}
-- How long to wait before trying again when Rojo Manager does not answer.
local RETRY_SECONDS = 5
-- The largest poll Rojo Manager accepts, results included.
local MAX_POLL_BYTES = 1024 * 1024
-- Rojo's port when its port field is empty.
local DEFAULT_PORT = 34872
-- Paths a pending sync summary lists per kind of change.
local MAX_PENDING_PATHS = 20
-- How long connect with wait waits for Rojo to connect, ask to confirm, or fail.
local CONNECT_WAIT = 30
local SETTLED = { Connected = true, Confirming = true, Error = true }

-- Rojo renames the DataModel to the project's name on its first sync, and a playtest's copy of the
-- place is named "Game" without the DataModel's attributes. So the file name of a local place Rojo
-- Manager built ("MyGame-1a2b3c4d.rbxl") is kept in this attribute on ServerStorage, which goes into
-- the playtest server along with its attributes.
local PLACE_ATTRIBUTE = "RojoManagerPlace"

-- Remember a Rojo Manager local place's file name while game.Name still is it, which is right after
-- Studio opens the file. Places the user opened themselves are left untouched.
local function keepPlaceName()
	local built = string.match(game.Name, "%-%x%x%x%x%x%x%x%x%.rbxl$") ~= nil
	if game.PlaceId == 0 and built then
		game:GetService("ServerStorage"):SetAttribute(PLACE_ATTRIBUTE, game.Name)
	end
end

-- The name of an unpublished place, which Rojo Manager matches to a local place's file.
local function placeName(): string?
	if game.PlaceId ~= 0 then
		return nil
	end
	local kept = game:GetService("ServerStorage"):GetAttribute(PLACE_ATTRIBUTE)
	return if type(kept) == "string" then kept else game.Name
end

-- Whether to back up Studio-only instances before Rojo removes them: Rojo Manager's setting, from
-- its last poll answer.
local backupsEnabled = true

-- The instance at a full name from GetFullName ("Workspace.Map"), or nil. Names that contain dots
-- cannot be told apart from nesting, so those are not found.
local function findPath(path: string): Instance?
	local current: Instance = game
	for _, name in string.split(path, ".") do
		local child = current:FindFirstChild(name)
		if not child then
			return nil
		end
		current = child
	end
	return current
end

-- Send copies of instances Rojo is about to remove to Rojo Manager, which keeps them as a backup.
local function sendBackup(app, copies: { Instance }, paths: { string }, skipped: { string })
	local ok, data = pcall(function()
		local serialized = game:GetService("SerializationService"):SerializeInstancesAsync(copies)
		return buffer.tostring(game:GetService("EncodingService"):Base64Encode(serialized))
	end)
	for _, copy in copies do
		copy:Destroy()
	end
	local report = {
		placeId = game.PlaceId,
		placeName = placeName(),
		port = tonumber(app.port:getValue()) or DEFAULT_PORT,
		paths = paths,
		skipped = skipped,
		data = if ok then data else nil,
		error = if ok then nil else tostring(data),
	}
	if ok and #data > MAX_BACKUP_BYTES then
		report.data = nil
		report.error = `The copy is too large to send ({#data} bytes)`
	end
	local encoded, json = pcall(HttpService.JSONEncode, HttpService, report)
	if encoded then
		pcall(HttpService.RequestAsync, HttpService, {
			Url = BACKUP_URL,
			Method = "POST",
			Headers = { ["Content-Type"] = "application/json" },
			Body = json,
		})
	end
end

-- Before Rojo applies a patch, copy what it removes that exists only in Studio: instances Rojo does
-- not track, which arrive as Instances rather than ids. The instance map cannot tell them apart:
-- Rojo's own precommit hook runs first and gives them ids for its diff view (PatchTree.build).
-- Copying is instant; sending runs afterwards.
local function backUpRemovals(app, patch)
	if not backupsEnabled then
		return
	end
	local copies, paths, skipped = {}, {}, {}
	for _, removed in patch.removed do
		if typeof(removed) ~= "Instance" then
			continue
		end
		local path = removed:GetFullName()
		local ok, copy = pcall(removed.Clone, removed)
		if ok and copy then
			copy:SetAttribute(BACKUP_PATH_ATTRIBUTE, path)
			table.insert(copies, copy)
			table.insert(paths, path)
		else
			-- Terrain, or anything not Archivable.
			table.insert(skipped, path)
		end
	end
	if #paths > 0 or #skipped > 0 then
		task.spawn(sendBackup, app, copies, paths, skipped)
	end
end

-- The attached app's current poll loop, so a detached or reattached app's loop stops.
local attached: { [any]: {} } = {}
-- The attached app's Output connection.
local outputs: { [any]: RBXScriptConnection } = {}

-- Rojo's live session, if any.
local function connected(app): boolean
	return app.serveSession ~= nil and app.serveSession:getStatus() == "Connected"
end

-- An instance's path as Rojo Manager shows it: "Workspace.Map", or "game" for the DataModel.
local function fullName(instance: Instance): string
	return if instance == game then "game" else instance:GetFullName()
end

local function count(changes, path: string?)
	changes.count += 1
	if path and #changes.paths < MAX_PENDING_PATHS then
		table.insert(changes.paths, path)
	end
end

-- Rojo 7.6 and later: the PatchTree Rojo builds for its diff view, walked as plain tables. Only the
-- top of an added subtree is listed; everything under it is counted.
local function summarizeTree(children, parentPath: string?, parentAdded: boolean, summary)
	for _, node in children do
		if type(node) ~= "table" then
			continue
		end
		local path = if typeof(node.instance) == "Instance"
			then fullName(node.instance)
			elseif parentPath and parentPath ~= "game" then `{parentPath}.{node.name}`
			else tostring(node.name)
		if node.patchType == "Add" then
			count(summary.added, if parentAdded then nil else path)
		elseif node.patchType == "Edit" then
			count(summary.changed, path)
		elseif node.patchType == "Remove" then
			count(summary.removed, path)
		end
		if type(node.children) == "table" then
			summarizeTree(node.children, path, node.patchType == "Add", summary)
		end
	end
end

-- Rojo 7.5 and earlier: the PatchSet itself, with the instance map it refers to.
local function summarizePatch(patch, instanceMap, summary)
	local function addedPath(id): string
		local added = patch.added[id]
		local parent = instanceMap.fromIds[added.Parent]
		local parentPath = if patch.added[added.Parent]
			then addedPath(added.Parent)
			elseif parent then fullName(parent)
			else nil
		return if parentPath and parentPath ~= "game" then `{parentPath}.{added.Name}` else tostring(added.Name)
	end
	for id, added in patch.added do
		count(summary.added, if patch.added[added.Parent] then nil else addedPath(id))
	end
	for _, change in patch.updated do
		local instance = instanceMap.fromIds[change.id]
		count(summary.changed, if instance then fullName(instance) else nil)
	end
	for _, removed in patch.removed do
		local instance = if typeof(removed) == "Instance" then removed else instanceMap.fromIds[removed]
		count(summary.removed, if instance then fullName(instance) else nil)
	end
end

-- The summary of the patch Rojo is waiting on, kept while that patch is the same.
local pendingCache: { source: any, summary: any }? = nil

-- What accepting Rojo's initial sync would do in Studio: instances added, changed, and removed, each
-- counted with its first paths. Rojo's initial diff only removes children of synced instances that
-- the project does not have, so every removal is an instance that exists only in Studio.
-- nil when Rojo is not confirming, or its state has a shape this bridge does not know.
local function pendingSync(app)
	local state = app.state or {}
	local confirm = if type(state.confirmData) == "table" then state.confirmData else {}
	local tree = if type(state.patchTree) == "table" then state.patchTree else nil
	local source = tree or confirm.patch
	if state.appStatus ~= "Confirming" or type(source) ~= "table" then
		return nil
	end
	if pendingCache and pendingCache.source == source then
		return pendingCache.summary
	end
	local function changes()
		return { count = 0, paths = {} }
	end
	local summary = { removed = changes(), added = changes(), changed = changes() }
	local ok = pcall(function()
		if tree then
			summarizeTree(tree.ROOT.children, nil, false, summary)
		else
			summarizePatch(confirm.patch, confirm.instanceMap, summary)
		end
	end)
	if not ok then
		return nil
	end
	for _, kind in summary do
		table.sort(kind.paths)
	end
	pendingCache = { source = source, summary = summary }
	return summary
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
		pending = pendingSync(app),
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

-- A value as JSON can carry it; an Instance as its full name, and anything else as its text.
-- JSONEncode turns an Instance into null instead of failing, so plain values are picked by type.
local function encodable(value: any): any
	local kind = type(value)
	if kind == "nil" or kind == "boolean" or kind == "number" or kind == "string" then
		return value
	elseif kind == "table" and pcall(HttpService.JSONEncode, HttpService, value) then
		return value
	end
	return if typeof(value) == "Instance" then value:GetFullName() else tostring(value)
end

local actions = {
	status = status,
	-- Connect Rojo to the Rojo server on this computer at args.port. With args.wait, return once Rojo
	-- connects, asks to confirm the initial sync, or fails, instead of at once.
	connect = function(app, args)
		endSession(app)
		-- A stale error would read as this attempt's outcome; clear it as Rojo's Okay button does.
		if (app.state or {}).appStatus == "Error" then
			app:setState({ appStatus = "NotConnected" })
		end
		app.setHost("localhost")
		app.setPort(tostring(args.port))
		-- Starting can yield on the sync lock, so it runs on its own thread.
		task.spawn(app.startSession, app)
		local deadline = os.clock() + CONNECT_WAIT
		while args.wait == true and not SETTLED[status(app).status] and os.clock() < deadline do
			task.wait(0.1)
		end
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
	-- Put a backup's copies (args.data, base64 .rbxm) back under their old parents, as one undoable
	-- step. Copies whose parent is gone are left out and listed as missing.
	restore = function(_app, args)
		local data = game:GetService("EncodingService"):Base64Decode(buffer.fromstring(args.data))
		local copies = game:GetService("SerializationService"):DeserializeInstancesAsync(data)
		local history = game:GetService("ChangeHistoryService")
		local recording = history:TryBeginRecording("Rojo Manager: Restore backup")
		local restored, missing = {}, {}
		for _, copy in copies do
			local path = copy:GetAttribute(BACKUP_PATH_ATTRIBUTE)
			copy:SetAttribute(BACKUP_PATH_ATTRIBUTE, nil)
			local parentPath = if type(path) == "string" then string.match(path, "^(.*)%.[^%.]+$") else nil
			local parent = if parentPath then findPath(parentPath) else nil
			if parent then
				copy.Parent = parent
				table.insert(restored, path)
			else
				copy:Destroy()
				table.insert(missing, if type(path) == "string" then path else copy.Name)
			end
		end
		if recording then
			history:FinishRecording(recording, Enum.FinishRecordingOperation.Commit)
		end
		return { restored = restored, missing = missing }
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
		local report = {
			placeId = game.PlaceId,
			placeName = placeName(),
			port = connectedPort(app),
			rojo = status(app),
			result = result,
		}
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
		if body and type(body.backups) == "boolean" then
			backupsEnabled = body.backups
		end
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

-- Send this DataModel's Output to Rojo Manager: what it logged before the plugin loaded, then each
-- new line, batched for OUTPUT_DELAY. Nothing is sent while the Output is quiet.
local function forwardOutput(app, context: string)
	local pending = {}
	local scheduled = false
	local function send()
		scheduled = false
		local lines = pending
		pending = {}
		local ok, json = pcall(HttpService.JSONEncode, HttpService, {
			placeId = game.PlaceId,
			placeName = placeName(),
			context = context,
			lines = lines,
		})
		if ok then
			pcall(HttpService.RequestAsync, HttpService, {
				Url = OUTPUT_URL,
				Method = "POST",
				Headers = { ["Content-Type"] = "application/json" },
				Body = json,
			})
		end
	end
	local function add(text: string, messageType: Enum.MessageType)
		if #pending >= MAX_OUTPUT_LINES then
			table.remove(pending, 1)
		end
		-- Cut at a character boundary: half a UTF-8 character would make the whole batch fail to encode.
		local cut = utf8.offset(text, MAX_OUTPUT_TEXT + 1)
		if cut then
			text = string.sub(text, 1, cut - 1)
		end
		table.insert(pending, { text = text, kind = OUTPUT_KINDS[messageType] or "output" })
		if not scheduled then
			scheduled = true
			task.delay(OUTPUT_DELAY, send)
		end
	end
	add(`{context} started`, Enum.MessageType.MessageInfo)
	-- Only as much history as fits beside that first line.
	local history = LogService:GetLogHistory()
	for index = math.max(1, #history - MAX_OUTPUT_LINES + 2), #history do
		add(history[index].message, history[index].messageType)
	end
	outputs[app] = LogService.MessageOut:Connect(add)
end

local Bridge = {}

-- Called from App:init. Only the Edit-mode plugin is controllable; it and a playtest's server
-- forward Output.
function Bridge.attach(app)
	if not RunService:IsEdit() then
		if RunService:IsServer() then
			forwardOutput(app, "server")
		end
		return
	end
	keepPlaceName()
	forwardOutput(app, "edit")
	local token = {}
	attached[app] = token
	-- Deferred until App:init has finished, so the host and port the poll reports are set.
	task.defer(poll, app, token)
	task.spawn(reportRojoPlugins)
end

-- Called from App:startSession just before a session starts, so even its first sync is covered.
function Bridge.session(app, serveSession)
	serveSession:hookPrecommit(function(patch)
		backUpRemovals(app, patch)
	end)
end

-- Called from App:willUnmount when the plugin unloads.
function Bridge.detach(app)
	attached[app] = nil
	if outputs[app] then
		outputs[app]:Disconnect()
		outputs[app] = nil
	end
end

return Bridge
