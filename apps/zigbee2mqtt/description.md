Zigbee2MQTT talks to Zigbee devices (sensors, switches, bulbs, plugs, thermostats from
hundreds of brands) through one coordinator adapter, and publishes them on an MQTT broker.
Home Assistant and other automation software pick them up from there, with no vendor hub or
cloud account.

It needs an MQTT broker (install Mosquitto first) and a Zigbee coordinator: a USB stick
plugged into the NAS or a network coordinator.
