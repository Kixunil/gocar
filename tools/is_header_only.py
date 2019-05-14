#!/usr/bin/python

#from __future__ import print_function

import sys
import os
from pycparser import c_parser, c_ast, parse_file

class DeclVisitor(c_ast.NodeVisitor):
    def __init__(self, file_name):
        self.decls = set()
        self.file_name = file_name

    def visit_Decl(self, node):
        #print(node.name, node.type, node.funcspec, node.storage, node.init, node.bitsize, node.coord)
        if node.name is not None:
            if node.init is None and ("extern" in node.storage or isinstance(node.type, c_ast.FuncDecl)):
                if node.coord.file == self.file_name:
                    self.decls.add(node.name)
            else:
                try:
                    self.decls.remove(node.name)
                except(KeyError):
                    pass

    def visit_FuncDef(self, node):
        try:
            #print(node.decl.name)
            self.decls.remove(node.decl.name)
        except(KeyError):
            pass

dirname = os.path.dirname(sys.argv[0])
ast = parse_file(sys.argv[1], use_cpp=True, cpp_args=[r"-nostdinc", r"-I" + dirname + r"/fake_libc_include", r"-DPYCPARSER"] + sys.argv[2:])
visitor = DeclVisitor(sys.argv[1])

visitor.visit(ast)

#print(visitor.decls)

if len(visitor.decls) == 0:
    sys.exit(0)
else:
    sys.exit(1)
