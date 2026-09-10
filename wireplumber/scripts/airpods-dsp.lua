-- ! ฝัง stereo DSP ไว้ใน A2DP sink เดิม โดยไม่สร้าง virtual sink เพิ่ม

local raw_args = ...
local args = raw_args and raw_args:parse (1) or {}

local log = Log.open_topic ("s-airpods-dsp")
local metadata_name = args["metadata.name"] or "airpods-linux"
local address = args["device.address"] or ""
local hrtf_path = args["hrtf.path"] or
    "/usr/share/libmysofa/MIT_KEMAR_normal_pinna.sofa"
local graph_key = "audioconvert.filter-graph.7"

local valid_modes = {
  off = true,
  wide = true,
  fix = true,
  spatial = true,
}

local state = State ("airpods-linux-dsp")
local state_table = state:load ()
local current_mode = state_table["mode"] or "off"
local current_yaw = 0.0
local current_pitch = 0.0
local control_metadata = nil
local tracked_nodes = {}

if not valid_modes[current_mode] then
  current_mode = "off"
end

local function graph_json ()
  return Json.Object {
    nodes = Json.Array {
      Json.Object {
        type = "builtin",
        label = "copy",
        name = "input_l",
      },
      Json.Object {
        type = "builtin",
        label = "copy",
        name = "input_r",
      },
      Json.Object {
        type = "sofa",
        label = "spatializer",
        name = "sp_l",
        config = Json.Object {
          filename = hrtf_path,
          blocksize = 64,
          tailsize = 2048,
          gain = 1.0,
        },
        control = Json.Object {
          Azimuth = 330.0,
          Elevation = 0.0,
          Radius = 1.0,
        },
      },
      Json.Object {
        type = "sofa",
        label = "spatializer",
        name = "sp_r",
        config = Json.Object {
          filename = hrtf_path,
          blocksize = 64,
          tailsize = 2048,
          gain = 1.0,
        },
        control = Json.Object {
          Azimuth = 30.0,
          Elevation = 0.0,
          Radius = 1.0,
        },
      },
      Json.Object {
        type = "builtin",
        label = "mixer",
        name = "mix_l",
        control = Json.Object {
          ["Gain 1"] = 1.0,
          ["Gain 2"] = 0.0,
          ["Gain 3"] = 0.0,
          ["Gain 4"] = 0.0,
        },
      },
      Json.Object {
        type = "builtin",
        label = "mixer",
        name = "mix_r",
        control = Json.Object {
          ["Gain 1"] = 1.0,
          ["Gain 2"] = 0.0,
          ["Gain 3"] = 0.0,
          ["Gain 4"] = 0.0,
        },
      },
    },
    links = Json.Array {
      Json.Object { output = "input_l:Out", input = "sp_l:In" },
      Json.Object { output = "input_r:Out", input = "sp_r:In" },
      Json.Object { output = "input_l:Out", input = "mix_l:In 1" },
      Json.Object { output = "input_r:Out", input = "mix_l:In 2" },
      Json.Object { output = "sp_l:Out L", input = "mix_l:In 3" },
      Json.Object { output = "sp_r:Out L", input = "mix_l:In 4" },
      Json.Object { output = "input_r:Out", input = "mix_r:In 1" },
      Json.Object { output = "input_l:Out", input = "mix_r:In 2" },
      Json.Object { output = "sp_l:Out R", input = "mix_r:In 3" },
      Json.Object { output = "sp_r:Out R", input = "mix_r:In 4" },
    },
    inputs = Json.Array { "input_l:In", "input_r:In" },
    outputs = Json.Array { "mix_l:Out", "mix_r:Out" },
  }:to_string ()
end

local function set_node_params (node, values)
  node:set_params ("Props", Pod.Object {
    "Spa:Pod:Object:Param:Props", "Props",
    params = Pod.Struct (values),
  })
end

local function normalized_azimuth (value)
  while value < 0.0 do
    value = value + 360.0
  end
  while value >= 360.0 do
    value = value - 360.0
  end
  return value
end

local function mode_controls ()
  local direct = 1.0
  local cross = 0.0
  local hrtf = 0.0
  local yaw = 0.0
  local pitch = 0.0

  if current_mode == "wide" then
    -- ? Mid/side แบบ normalize เพื่อขยาย stereo โดยไม่เพิ่ม peak gain
    direct = 0.87
    cross = -0.13
  elseif current_mode == "fix" or current_mode == "spatial" then
    direct = 0.0
    cross = 0.0
    hrtf = 0.65
    if current_mode == "spatial" then
      yaw = current_yaw
      pitch = current_pitch
    end
  end

  return {
    "mix_l:Gain 1", direct,
    "mix_l:Gain 2", cross,
    "mix_l:Gain 3", hrtf,
    "mix_l:Gain 4", hrtf,
    "mix_r:Gain 1", direct,
    "mix_r:Gain 2", cross,
    "mix_r:Gain 3", hrtf,
    "mix_r:Gain 4", hrtf,
    "sp_l:Azimuth", normalized_azimuth (330.0 - yaw),
    "sp_r:Azimuth", normalized_azimuth (30.0 - yaw),
    "sp_l:Elevation", -pitch,
    "sp_r:Elevation", -pitch,
  }
end

local function apply_mode (node)
  set_node_params (node, mode_controls ())
  log:info (node, "applied AirPods sound mode: " .. current_mode)
end

local function apply_mode_to_all_nodes ()
  for _, node in pairs (tracked_nodes) do
    apply_mode (node)
  end
end

local function matches_airpods (node)
  local properties = node.properties
  if properties["media.class"] ~= "Audio/Sink"
      or properties["device.api"] ~= "bluez5"
      or properties["api.bluez5.profile"] ~= "a2dp-sink"
      or properties["library.name"] ~= "audioconvert/libspa-audioconvert" then
    return false
  end

  if address ~= "" then
    return properties["api.bluez5.address"] == address
  end

  local description = properties["node.description"] or
      properties["device.description"] or ""
  return description:find ("AirPods", 1, true) ~= nil
end

local function decode_string (value)
  if value == nil then
    return nil
  end
  return value:match ('^"(.*)"$') or value
end

local function store_mode ()
  state_table["mode"] = current_mode
  state:save_after_timeout (state_table)
end

local function publish_mode ()
  if control_metadata then
    control_metadata:set (
        0, "sound.mode", "Spa:String:JSON", '"' .. current_mode .. '"')
  end
end

local function metadata_changed (_metadata, subject, key, _value_type, value)
  if subject ~= 0 then
    return
  end

  if key == "sound.mode" then
    local requested = decode_string (value)
    if not valid_modes[requested] then
      log:warning ("ignoring invalid AirPods sound mode: " .. tostring (requested))
      publish_mode ()
      return
    end
    if requested ~= current_mode then
      current_mode = requested
      store_mode ()
      apply_mode_to_all_nodes ()
    end
  elseif key == "sound.yaw" then
    current_yaw = math.max (-120.0, math.min (120.0, tonumber (value) or 0.0))
    if current_mode == "spatial" then
      apply_mode_to_all_nodes ()
    end
  elseif key == "sound.pitch" then
    current_pitch = math.max (-45.0, math.min (45.0, tonumber (value) or 0.0))
    if current_mode == "spatial" then
      apply_mode_to_all_nodes ()
    end
  end
end

metadata_om = ObjectManager {
  Interest {
    type = "metadata",
    Constraint { "metadata.name", "=", metadata_name },
  },
}

metadata_om:connect ("object-added", function (_, metadata)
  control_metadata = metadata
  metadata:connect ("changed", metadata_changed)
  publish_mode ()
end)

metadata_om:connect ("object-removed", function (_, metadata)
  if metadata == control_metadata then
    control_metadata = nil
  end
end)

nodes_om = ObjectManager {
  Interest {
    type = "node",
    Constraint { "media.class", "=", "Audio/Sink", type = "pw-global" },
    Constraint { "device.api", "=", "bluez5", type = "pw" },
    Constraint { "api.bluez5.profile", "=", "a2dp-sink", type = "pw" },
    Constraint {
      "library.name", "=", "audioconvert/libspa-audioconvert", type = "pw"
    },
  },
}

nodes_om:connect ("object-added", function (_, node)
  if not matches_airpods (node) then
    return
  end
  tracked_nodes[node.id] = node
  set_node_params (node, { graph_key, graph_json () })
  Core.sync (function ()
    if tracked_nodes[node.id] == node then
      apply_mode (node)
    end
  end)
end)

nodes_om:connect ("object-removed", function (_, node)
  tracked_nodes[node.id] = nil
end)

metadata_om:activate ()
nodes_om:activate ()
