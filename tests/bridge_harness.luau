-- Runs the real Rojo Manager bridge (rojo_bridge/RojoManagerBridge.luau) outside Studio, for the
-- end-to-end test. Studio's services and Rojo's plugin App are small stand-ins: HttpService makes
-- real requests, so the bridge talks to a real Rojo Manager, and "connecting" Rojo only records
-- the server it was asked to use.
-- Usage: lune run bridge_harness.luau <RojoManagerBridge.luau> <placeId> [bridge port]
-- Prints one line per event ("connect <port>", "disconnect") for the test to read.

local fs = require("@lune/fs")
local luau = require("@lune/luau")
local net = require("@lune/net")
local process = require("@lune/process")
local serde = require("@lune/serde")
local stdio = require("@lune/stdio")
local task = require("@lune/task")

local bridgePath, placeId, port = process.args[1], tonumber(process.args[2]), process.args[3]
assert(bridgePath and placeId, "Usage: lune run bridge_harness.luau <RojoManagerBridge.luau> <placeId> [port]")

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

local services = {
	HttpService = HttpService,
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

local game = {
	PlaceId = placeId,
	GetService = function(_, name)
		return assert(services[name], `No stand-in for {name}`)
	end,
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
		Enum = { MessageType = MessageType },
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
		end,
	},
	setHost = function(host)
		app.host.current = host
	end,
	setPort = function(newPort)
		app.port.current = newPort
	end,
	startSession = function()
		app.serveSession = {
			getStatus = function()
				return "Connected"
			end,
		}
		app.state = { appStatus = "Connected", address = `localhost:{app.port.current}`, projectName = "MyGame" }
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
