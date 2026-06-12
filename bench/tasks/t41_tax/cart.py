from constants import TAX_RATE

def total(subtotal):
    return round(subtotal * (1 + TAX_RATE), 2)
