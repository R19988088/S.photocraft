"""Reject a startup dependency on the optional Windows PDF component."""
import struct
import sys
from pathlib import Path


def imports(path):
    data = Path(path).read_bytes()
    pe = struct.unpack_from('<I', data, 0x3C)[0]
    assert data[pe:pe + 4] == b'PE\0\0', f'{path}: not a PE file'
    sections = struct.unpack_from('<H', data, pe + 6)[0]
    optional_size = struct.unpack_from('<H', data, pe + 20)[0]
    optional = pe + 24
    magic = struct.unpack_from('<H', data, optional)[0]
    assert magic in (0x10B, 0x20B), f'{path}: unsupported PE format'
    directories = optional + (112 if magic == 0x20B else 96)
    import_rva = struct.unpack_from('<I', data, directories + 8)[0]
    section_table = optional + optional_size

    def offset(rva):
        for i in range(sections):
            size, start, raw_size, raw = struct.unpack_from('<IIII', data, section_table + i * 40 + 8)
            if start <= rva < start + max(size, raw_size):
                return raw + rva - start
        raise ValueError(f'{path}: unmapped RVA {rva:#x}')

    names = []
    if import_rva:
        descriptor = offset(import_rva)
        while any(data[descriptor:descriptor + 20]):
            name = offset(struct.unpack_from('<I', data, descriptor + 12)[0])
            names.append(data[name:data.index(b'\0', name)].decode('ascii').lower())
            descriptor += 20
    return names


if __name__ == '__main__':
    for path in sys.argv[1:]:
        names = imports(path)
        assert 'windows.data.pdf.dll' not in names, f'{path}: PDF DLL still required at startup'
        print(f'{Path(path).name}: no startup dependency on windows.data.pdf.dll ({len(names)} DLL imports)')
