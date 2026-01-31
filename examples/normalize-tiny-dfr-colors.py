#!/usr/bin/env python3
"""
Post-processing script for matugen-generated tiny-dfr configs.
Converts raw RGB values (0-255) to normalized RGB values (0.0-1.0).
"""

import sys
import re

def normalize_rgb_values(input_file, output_file):
    """Convert RGB values from 0-255 range to 0.0-1.0 range."""
    with open(input_file, 'r') as f:
        content = f.read()

    # Regex to find RGB arrays like [R, G, B] where R, G, B are integers
    pattern = re.compile(r'\[(\d+),\s*(\d+),\s*(\d+)\]')

    def replace_rgb(match):
        r = int(match.group(1)) / 255.0
        g = int(match.group(2)) / 255.0
        b = int(match.group(3)) / 255.0
        return f'[{r:.3f}, {g:.3f}, {b:.3f}]'

    new_content = pattern.sub(replace_rgb, content)

    with open(output_file, 'w') as f:
        f.write(new_content)

    print(f"✅ Successfully normalized RGB values from {input_file} to {output_file}")

if __name__ == "__main__":
    if len(sys.argv) != 3:
        print("Usage: python3 normalize-tiny-dfr-colors.py <input_toml_file> <output_toml_file>")
        sys.exit(1)

    input_toml = sys.argv[1]
    output_toml = sys.argv[2]
    normalize_rgb_values(input_toml, output_toml)



