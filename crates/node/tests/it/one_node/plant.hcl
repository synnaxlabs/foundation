# The time of each sample from the OPC UA server.
channel "plant.time" { kind = "index" }

# A value that spikes now and then.
channel "plant.spike" {
  data_type = "f64"
  index     = "plant.time"
}

# A value that dips now and then.
channel "plant.dip" {
  data_type = "f64"
  index     = "plant.time"
}

# A value that rises.
channel "plant.trend" {
  data_type = "f64"
  index     = "plant.time"
}

# On node "edge", reads three values from the OPC UA server.
connector "plc" {
  kind    = "opcua"
  node    = "edge"
  address = "opc.tcp://localhost:50000"
  read "plant.spike" { node_id = "ns=3;s=SpikeData" }
  read "plant.dip" { node_id = "ns=3;s=DipData" }
  read "plant.trend" { node_id = "ns=3;s=PositiveTrendData" }
}

# On node "edge", pushes each sample to InfluxDB, and keeps it while InfluxDB is down.
connector "influx" {
  kind        = "influx"
  node        = "edge"
  address     = "http://localhost:8086"
  database    = "plant"
  measurement = "plant"
  select      = "plant.*"
}
