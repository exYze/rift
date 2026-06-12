def chunk(xs, n):
    return [xs[i*n:(i+1)*n] for i in range(len(xs) // n)]
