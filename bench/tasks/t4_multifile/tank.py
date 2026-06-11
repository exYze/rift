from geometry import circle_area

def tank_volume(r, h):
    """Volume of a cylinder, using an accurate circle area."""
    return round(circle_area(r) * h, 2)

if __name__ == "__main__":
    print(tank_volume(1, 1))
