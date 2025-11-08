import json


def load_json(path: str):
    with open(path, 'r') as f:
        return json.load(f)


def extract_coordinates(data):
    coordinates = []

    # New format: data is a list of steps with 'to' -> { 'max': [x, y, plane] }
    if isinstance(data, list):
        for step in data:
            if not isinstance(step, dict):
                continue
            to = step.get('to')
            if isinstance(to, dict):
                max_coords = to.get('max')
                if isinstance(max_coords, (list, tuple)) and len(max_coords) >= 3:
                    x, y, plane = max_coords[0], max_coords[1], max_coords[2]
                    coordinates.append((x, y, plane))

    # Old format: data has key 'actions' containing objects with 'to' -> { 'max': [x, y, plane] }
    elif isinstance(data, dict):
        for action in data.get('actions', []):
            if not isinstance(action, dict):
                continue
            to = action.get('to')
            if isinstance(to, dict):
                max_coords = to.get('max')
                if isinstance(max_coords, (list, tuple)) and len(max_coords) >= 3:
                    x, y, plane = max_coords[0], max_coords[1], max_coords[2]
                    coordinates.append((x, y, plane))

    return coordinates


def to_java_array(coordinates):
    java_code = "Coordinate[] path = {\n"
    for i, (x, y, plane) in enumerate(coordinates):
        java_code += f"    new Coordinate({int(x)}, {int(y)}, {int(plane)})"
        if i < len(coordinates) - 1:
            java_code += ","
        java_code += "\n"
    java_code += "};"
    return java_code


def main():
    data = load_json('result.json')
    coordinates = extract_coordinates(data)
    java_code = to_java_array(coordinates)

    # Write to results_parsed.json
    with open('results_parsed.json', 'w') as f:
        f.write(java_code)


if __name__ == '__main__':
    main()
