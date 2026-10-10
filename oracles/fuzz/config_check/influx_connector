connector "influx" {
  kind = "influx"
  node = "cloud"
  address = "http://influx:8086"
  select = "edge.*"
  reader {
    mode = "complete"
    hold = "2h"
  }
}
