#!/bin/bash
[ "$(python3 app.py)" = "DATA:X" ] && ! grep -q fetch_data client.py app.py
