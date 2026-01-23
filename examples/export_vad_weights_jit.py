import torch
import struct
import numpy as np
from silero_vad import load_silero_vad
import os

def export_jit_weights(output_path):
    print("Loading JIT model...")
    model = load_silero_vad()
    
    # Access 16k inner model
    inner = model._model
    sd = inner.state_dict()
    
    weights_map = {
        'enc0_weight': 'encoder.0.reparam_conv.weight',
        'enc0_bias': 'encoder.0.reparam_conv.bias',
        'enc1_weight': 'encoder.1.reparam_conv.weight',
        'enc1_bias': 'encoder.1.reparam_conv.bias',
        'enc2_weight': 'encoder.2.reparam_conv.weight',
        'enc2_bias': 'encoder.2.reparam_conv.bias',
        'enc3_weight': 'encoder.3.reparam_conv.weight',
        'enc3_bias': 'encoder.3.reparam_conv.bias',
        'lstm0_w_ih': 'decoder.rnn.weight_ih',
        'lstm0_w_hh': 'decoder.rnn.weight_hh',
        'lstm0_b_ih': 'decoder.rnn.bias_ih',
        'lstm0_b_hh': 'decoder.rnn.bias_hh',
        # No lstm1 in JIT V4/Default
        'out_weight': 'decoder.decoder.2.weight',
        'out_bias': 'decoder.decoder.2.bias'
    }
    
    found_weights = {}
    
    for target_name, source_key in weights_map.items():
        if source_key in sd:
            t = sd[source_key]
            # Convert to numpy float32 flat
            data = t.cpu().numpy().flatten().astype(np.float32)
            found_weights[target_name] = data
            print(f"Loaded {target_name}: {t.shape} -> {data.shape}")
        else:
            print(f"WARNING: Key {source_key} not found in state_dict")

    # Handle output path
    abs_out = os.path.abspath(output_path)
    print(f"Writing to {abs_out}")
    with open(abs_out, 'wb') as f:
        # Header: Num tensors
        f.write(struct.pack('<I', len(found_weights)))
        
        for name, data in found_weights.items():
            # Name len
            name_bytes = name.encode('utf-8')
            f.write(struct.pack('<I', len(name_bytes)))
            f.write(name_bytes)
            
            # Shape len (0 for flat)
            f.write(struct.pack('<I', 0))
            
            # Data len (bytes)
            data_bytes = data.tobytes()
            f.write(struct.pack('<I', len(data_bytes)))
            f.write(data_bytes)

import sys

if __name__ == "__main__":
    if len(sys.argv) > 1:
        path = sys.argv[1]
    else:
        path = "src/media/vad/silero_weights.bin"
    export_jit_weights(path)
