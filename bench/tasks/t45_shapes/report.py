from shapes import rect_area, circle_perimeter

def total_area(items):
    out = 0.0
    for it in items:
        if it[0] == 'rect':
            out += rect_area(it[1], it[2])
        elif it[0] == 'circle':
            out += circle_perimeter(it[1])
    return out
