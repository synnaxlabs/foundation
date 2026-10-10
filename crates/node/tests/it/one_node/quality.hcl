channel "plant.time" { kind = "index" }

# The OPC UA status of each sample of `plant.spike`.
channel "plant.quality" {
  data_type = "quality"
  index     = "plant.time"
}

channel "plant.spike" {
  data_type = "f64"
  index     = "plant.time"
  quality   = "plant.quality"
}

connector "plc" {
  kind    = "opcua"
  node    = "edge"
  address = "opc.tcp://localhost:50000"
  read "plant.spike" { node_id = "ns=3;s=SpikeData" }
}

connector "influx" {
  kind        = "influx"
  node        = "edge"
  address     = "http://localhost:8086"
  database    = "plant"
  measurement = "plant"
  select      = "plant.*"
}
