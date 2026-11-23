-- Puts instances captured from Studio into a place built from the project, for `rojo syncback`.
-- Usage: lune run assemble_place.luau <base.rbxl> <capture.rbxm> <manifest.json> <output.rbxl>
--
-- The manifest lists, in the capture's order, what each run of its root instances is:
-- {"path": ["ReplicatedStorage", "Shared"], "count": 1} replaces that instance, and with
-- "children": true the roots replace the children of the instance at path, which is how a service
-- is captured. Everything else in the base place stays as the project built it, so syncback finds
-- no difference there.

local fs = require("@lune/fs")
local process = require("@lune/process")
local roblox = require("@lune/roblox")
local serde = require("@lune/serde")

local basePath, capturePath, manifestPath, output = table.unpack(process.args)
if not (basePath and capturePath and manifestPath and output) then
	error("Usage: lune run assemble_place.luau <base.rbxl> <capture.rbxm> <manifest.json> <output.rbxl>", 0)
end

local game = roblox.deserializePlace(fs.readFile(basePath))
-- No capture file: every entry is a service left without children.
local captured = if fs.isFile(capturePath) then roblox.deserializeModel(fs.readFile(capturePath)) else {}
local manifest = serde.decode("json", fs.readFile(manifestPath))

-- The instance at `path` in the base place, down to its first `length` names.
local function find(path: { string }, length: number)
	local current = game
	for index = 1, length do
		local child = current:FindFirstChild(path[index])
		if not child then
			error(`The project builds no {table.concat(path, ".", 1, index)}`, 0)
		end
		current = child
	end
	return current
end

local next = 1
for _, entry in manifest do
	local roots = table.move(captured, next, next + entry.count - 1, 1, {})
	next += entry.count
	if #roots ~= entry.count then
		error("The capture holds fewer instances than its manifest lists", 0)
	end
	if entry.children then
		local target = find(entry.path, #entry.path)
		for _, child in target:GetChildren() do
			child:Destroy()
		end
		for _, root in roots do
			root.Parent = target
		end
	else
		local parent = find(entry.path, #entry.path - 1)
		local old = parent:FindFirstChild(entry.path[#entry.path])
		if old then
			old:Destroy()
		end
		roots[1].Parent = parent
	end
end

fs.writeFile(output, roblox.serializePlace(game))
