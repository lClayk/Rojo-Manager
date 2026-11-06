-- SPDX-License-Identifier: MPL-2.0
-- Adds the Rojo Manager bridge to an official Rojo plugin model downloaded from Rojo's releases.
-- Usage: lune run patch_rojo_plugin.luau <Rojo.rbxm> <RojoManagerBridge.luau> <output.rbxm>
-- Fails without writing anything when the plugin layout is not the one this patch expects.

local fs = require("@lune/fs")
local process = require("@lune/process")
local roblox = require("@lune/roblox")

local input, bridgePath, output = process.args[1], process.args[2], process.args[3]
if not (input and bridgePath and output) then
	error("Usage: lune run patch_rojo_plugin.luau <Rojo.rbxm> <RojoManagerBridge.luau> <output.rbxm>", 0)
end

local roots = roblox.deserializeModel(fs.readFile(input))
local plugin = roots[1] and roots[1]:FindFirstChild("Plugin")
local app = plugin and plugin:FindFirstChild("App")
if not (app and app:IsA("ModuleScript")) then
	error("Unsupported Rojo plugin: Plugin.App was not found", 0)
end
if plugin:FindFirstChild("RojoManagerBridge") then
	error("This Rojo plugin already contains the Rojo Manager bridge", 0)
end

local function insertAfter(source: string, anchor: string, line: string): string
	local first, last = string.find(source, anchor, 1, true)
	if not first or string.find(source, anchor, last + 1, true) then
		error(`Unsupported Rojo version: expected exactly one "{anchor}" in Plugin.App`, 0)
	end
	return string.sub(source, 1, last) .. "\n" .. line .. string.sub(source, last + 1)
end

local source = app.Source
source = insertAfter(source, "function App:init()", "\trequire(script.Parent.RojoManagerBridge).attach(self)")
source = insertAfter(source, "function App:willUnmount()", "\trequire(script.Parent.RojoManagerBridge).detach(self)")
app.Source = source

local bridge = roblox.Instance.new("ModuleScript")
bridge.Name = "RojoManagerBridge"
bridge.Source = fs.readFile(bridgePath)
bridge.Parent = plugin

fs.writeFile(output, roblox.serializeModel(roots))
print(`Added the Rojo Manager bridge to {output}`)
