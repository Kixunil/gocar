#!/bin/bash

function fail() {
	echo "Failed: $1"
	exit 1
}

DIR=`dirname $0`

for FILE in "$DIR"/pass/*.h;
do
	"$DIR"/../is_header_only.py "$FILE" || fail "$FILE"
done

for FILE in "$DIR"/fail/*.h;
do
	"$DIR"/../is_header_only.py "$FILE" && fail "$FILE"
done

exit 0
