-- Runs the real Rojo Manager bridge (rojo_bridge/RojoManagerBridge.luau) outside Studio, for the
-- end-to-end test. Studio's services and Rojo's plugin App are small stand-ins: HttpService makes
-- real requests, so the bridge talks to a real Rojo Manager, and "connecting" Rojo only records
-- the server it was asked to use.
-- Usage: lune run bridge_harness.luau <RojoManagerBridge.luau> <placeId>[:<place name>] [bridge port] [confirm]
-- The place name is game.Name, which Studio sets to a local place's file name (place ID 0). With
-- confirm, connecting waits for the initial sync to be confirmed, with a pending patch as Rojo 7.6
-- and later keep it (state.patchTree). Accepting it runs the precommit hooks the bridge registered,
-- with a patch that removes a Studio-only Workspace.Map and Workspace.Terrain, which cannot be copied.
-- Prints one line per event ("connect <port>", "disconnect") for the test to read.

local fs = require("@lune/fs")
local luau = require("@lune/luau")
local net = require("@lune/net")
local process = require("@lune/process")
local serde = require("@lune/serde")
local stdio = require("@lune/stdio")
local task = require("@lune/task")

local bridgePath, place, port, confirm = process.args[1], process.args[2] or "", process.args[3], process.args[4] == "confirm"
local placeId, placeName = string.match(place, "^(%d+):?(.*)$")
placeId = tonumber(placeId)
assert(bridgePath and placeId, "Usage: lune run bridge_harness.luau <RojoManagerBridge.luau> <placeId>[:<name>] [port]")

local function event(text: string)
	stdio.write(text .. "\n")
end

local MessageType = {
	MessageOutput = { Name = "MessageOutput" },
	MessageInfo = { Name = "MessageInfo" },
	MessageWarning = { Name = "MessageWarning" },
	MessageError = { Name = "MessageError" },
}

local listeners = {}
local LogService = {
	MessageOut = {
		Connect = function(_, listener)
			table.insert(listeners, listener)
			return {
				Disconnect = function()
					table.remove(listeners, table.find(listeners, listener))
				end,
			}
		end,
	},
	GetLogHistory = function()
		return { { message = "logged before the plugin loaded", messageType = MessageType.MessageOutput } }
	end,
}

local function log(message: string, messageType)
	for _, listener in table.clone(listeners) do
		listener(message, messageType)
	end
end

local HttpService = {
	JSONEncode = function(_, value)
		return serde.encode("json", value)
	end,
	JSONDecode = function(_, text)
		return serde.decode("json", text)
	end,
	RequestAsync = function(_, request)
		local response = net.request({
			url = request.Url,
			method = request.Method,
			headers = request.Headers,
			body = request.Body,
		})
		return { Success = response.ok, StatusCode = response.statusCode, Body = response.body }
	end,
}

-- Standard base64, for the EncodingService stand-in.
local ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"

local function base64Encode(text: string): string
	return (
		string.gsub(text .. "\0\0", "(...)", function(chunk)
			local a, b, c = string.byte(chunk, 1, 3)
			local n = a * 65536 + b * 256 + c
			local out = ""
			for shift = 18, 0, -6 do
				local index = bit32.band(bit32.rshift(n, shift), 63) + 1
				out ..= string.sub(ALPHABET, index, index)
			end
			return out
		end)
	)
end

local function encode(text: string): string
	local padding = (3 - #text % 3) % 3
	local encoded = string.sub(base64Encode(text), 1, math.ceil(#text / 3) * 4)
	return string.sub(encoded, 1, #encoded - padding) .. string.rep("=", padding)
end

local function decode(text: string): string
	text = string.gsub(text, "=", "")
	local bits, count, out = 0, 0, {}
	for index = 1, #text do
		bits = bits * 64 + (string.find(ALPHABET, string.sub(text, index, index), 1, true) - 1)
		count += 6
		if count >= 8 then
			count -= 8
			table.insert(out, string.char(bit32.band(bit32.rshift(bits, count), 255)))
			bits = bit32.band(bits, bit32.lshift(1, count) - 1)
		end
	end
	return table.concat(out)
end

-- A copy of a Studio instance, as Clone and DeserializeInstancesAsync make them.
local function copyOf(name: string, attributes: { [string]: any })
	local copy = { __instance = true, Name = name, attributes = attributes }
	function copy:SetAttribute(key, value)
		self.attributes[key] = value
	end
	function copy:GetAttribute(key)
		return self.attributes[key]
	end
	function copy:Destroy() end
	return copy
end

local services = {
	HttpService = HttpService,
	-- "Serializes" copies as JSON, which is all the round trip through Rojo Manager needs.
	SerializationService = {
		SerializeInstancesAsync = function(_, copies)
			local list = {}
			for _, copy in copies do
				table.insert(list, { name = copy.Name, attributes = copy.attributes })
			end
			return buffer.fromstring(serde.encode("json", list))
		end,
		DeserializeInstancesAsync = function(_, data)
			local copies = {}
			for _, item in serde.decode("json", buffer.tostring(data)) do
				table.insert(copies, copyOf(item.name, item.attributes))
			end
			return copies
		end,
	},
	EncodingService = {
		Base64Encode = function(_, data)
			return buffer.fromstring(encode(buffer.tostring(data)))
		end,
		Base64Decode = function(_, data)
			return buffer.fromstring(decode(buffer.tostring(data)))
		end,
	},
	ChangeHistoryService = {
		TryBeginRecording = function()
			return "recording"
		end,
		FinishRecording = function() end,
	},
	LogService = LogService,
	RunService = {
		IsEdit = function()
			return true
		end,
		IsServer = function()
			return false
		end,
	},
	PluginGuiService = {
		GetChildren = function()
			return {}
		end,
	},
}

local attributes = {}
services.ServerStorage = {
	GetAttribute = function(_, name)
		return attributes[name]
	end,
	SetAttribute = function(_, name, value)
		attributes[name] = value
	end,
}
local game = {
	__instance = true,
	PlaceId = placeId,
	Name = if placeName ~= "" then placeName else "Place1",
	GetService = function(_, name)
		return assert(services[name], `No stand-in for {name}`)
	end,
}

-- Workspace, for restore to find a backup's parent.
local workspace = { __instance = true, Name = "Workspace" }
function workspace:FindFirstChild()
	return nil
end
function game:FindFirstChild(name)
	return if name == "Workspace" then workspace else nil
end

-- What the confirmed sync removes: a Studio-only model, and Terrain, which cannot be copied.
local function studioOnly(name: string, copyable: boolean)
	local removed = { __instance = true, Name = name, ClassName = "Model" }
	function removed:GetFullName()
		return `Workspace.{name}`
	end
	function removed:Clone()
		assert(copyable, "Terrain cannot be cloned")
		return copyOf(name, {})
	end
	return removed
end
local REMOVED = { studioOnly("Map", true), studioOnly("Terrain", false) }

-- Rojo's first sync renames the DataModel to the project's name, as the real one does.
local function synced()
	game.Name = "MyGame"
end

-- A stand-in Instance: the bridge only asks for its full name.
local function instance(fullName: string)
	return {
		__instance = true,
		GetFullName = function()
			return fullName
		end,
	}
end

-- Studio's typeof, which also names the stand-in Instances.
local function studioTypeof(value: any): string
	if type(value) == "table" and value.__instance then
		return "Instance"
	end
	return typeof(value)
end

-- The PatchTree Rojo builds for its diff view: one Studio-only model removed, one script changed,
-- and a new folder with a script in it.
local PATCH_TREE = {
	ROOT = {
		children = {
			root = {
				instance = game,
				children = {
					workspace = {
						instance = instance("Workspace"),
						children = {
							map = { patchType = "Remove", name = "Map", instance = instance("Workspace.Map"), children = {} },
						},
					},
					scripts = {
						instance = instance("ServerScriptService"),
						children = {
							main = {
								patchType = "Edit",
								name = "main",
								instance = instance("ServerScriptService.main"),
								children = {},
							},
							tools = {
								patchType = "Add",
								name = "Tools",
								children = { hammer = { patchType = "Add", name = "Hammer", children = {} } },
							},
						},
					},
				},
			},
		},
	},
}

local source = fs.readFile(bridgePath)
if port then
	source = string.gsub(source, "localhost:34871", `localhost:{port}`)
end

-- Studio's loadstring, which the bridge's luau action uses for agent code.
local function loadstring(code: string, name: string?)
	local ok, chunk = pcall(luau.load, code, { debugName = name })
	if ok then
		return chunk
	end
	return nil, chunk
end

local Bridge = luau.load(source, {
	debugName = "RojoManagerBridge",
	environment = {
		game = game,
		typeof = studioTypeof,
		Enum = { MessageType = MessageType, FinishRecordingOperation = { Commit = "Commit" } },
		task = task,
		Random = {
			new = function()
				return {
					NextInteger = function(_, low, high)
						return math.random(low, high)
					end,
				}
			end,
		},
		loadstring = loadstring,
	},
})()

-- Rojo's plugin App, as far as the bridge reads and drives it.
local function value(initial: string)
	return {
		current = initial,
		getValue = function(self)
			return self.current
		end,
	}
end

local app
app = {
	state = { appStatus = "NotConnected" },
	host = value("localhost"),
	port = value(""),
	serveSession = nil,
	confirmationBindable = {
		Fire = function(_, choice)
			event(`confirm {choice}`)
			if app.state.appStatus ~= "Confirming" then
				return
			elseif choice == "Accept" then
				for _, hook in app.serveSession.precommit do
					-- Rojo's own hook, registered first, has given the removals ids for its diff view.
					local map = { fromInstances = { [REMOVED[1]] = "id-1", [REMOVED[2]] = "id-2" }, fromIds = {} }
					hook({ removed = REMOVED, added = {}, updated = {} }, map)
				end
				app.state = { appStatus = "Connected", address = `localhost:{app.port.current}`, projectName = "MyGame" }
				synced()
				event(`connect {app.port.current}`)
			else
				app.serveSession = nil
				app.state = { appStatus = "NotConnected" }
			end
		end,
	},
	setHost = function(host)
		app.host.current = host
	end,
	setPort = function(newPort)
		app.port.current = newPort
	end,
	setState = function(self, changes)
		-- As Roact's: a function maps the state to its changes, or to nil for none.
		if type(changes) == "function" then
			changes = changes(self.state)
			if changes == nil then
				return
			end
		end
		local state = table.clone(self.state)
		for key, value in changes do
			state[key] = value
		end
		self.state = state
	end,
	startSession = function()
		local session
		session = {
			precommit = {},
			-- Rojo's own session status: Disconnected once it is no longer the app's session.
			getStatus = function()
				if app.serveSession ~= session then
					return "Disconnected"
				end
				return if app.state.appStatus == "Connected" then "Connected" else "Connecting"
			end,
		}
		function session:hookPrecommit(hook)
			table.insert(self.precommit, hook)
		end
		function session:onStatusChanged() end
		-- As the patched App:startSession does, before the session starts.
		app.serveSession = session
		Bridge.session(app, session)
		if confirm then
			app.state = { appStatus = "Confirming", patchTree = PATCH_TREE, confirmData = {} }
			event(`confirming {app.port.current}`)
			return
		end
		app.state = { appStatus = "Connected", address = `localhost:{app.port.current}`, projectName = "MyGame" }
		synced()
		event(`connect {app.port.current}`)
	end,
	endSession = function()
		app.serveSession = nil
		app.state = { appStatus = "NotConnected" }
		event("disconnect")
	end,
}

Bridge.attach(app)
log("e2e print", MessageType.MessageOutput)
log("e2e warning", MessageType.MessageWarning)
log("e2e error", MessageType.MessageError)
event("attached")
