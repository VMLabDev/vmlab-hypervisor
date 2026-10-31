import unittest
import compile_database as database


class CompileDatabaseTests(unittest.TestCase):
    def test_cmake_output_field_is_optional(self):
        entry = {"file": "rts.c", "directory": "/build",
                 "command": "cc -o sail_runtime/CMakeFiles/rts.dir/rts.c.o -c rts.c"}
        self.assertEqual(database.object_output(entry), "sail_runtime/CMakeFiles/rts.dir/rts.c.o")
        self.assertEqual(database.select_entries([entry]), [entry])
        modern = {**entry, "command": "cc -o CMakeFiles/sail_runtime.dir/rts.c.o -c rts.c"}
        self.assertEqual(database.select_entries([modern]), [modern])

    def test_argument_array_and_quoted_output(self):
        entry = {"file": "sail_riscv_model.cpp", "directory": "/build",
                 "arguments": ["c++", "-o", "a directory/model.o", "-c", "model.cpp"]}
        self.assertEqual(database.object_output(entry), "a directory/model.o")
        self.assertEqual(database.select_entries([entry]), [entry])
        entry = {"file": "other.cpp", "command": "c++ -o 'a directory/other.o' -c other.cpp"}
        self.assertEqual(database.object_output(entry), "a directory/other.o")
        self.assertEqual(database.select_entries([entry]), [])

    def test_missing_object_output_is_explicit(self):
        with self.assertRaisesRegex(ValueError, "no object output"):
            database.object_output({"file": "model.cpp", "command": "c++ -c model.cpp"})


if __name__ == "__main__":
    unittest.main()
